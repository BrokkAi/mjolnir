use super::*;
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use mj_core::elicitation::ElicitationOption;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn request(kind: ElicitationFieldKind, required: bool) -> ElicitationRequest {
    ElicitationRequest {
        id: "ask-1".into(),
        message: "Choose an architecture".into(),
        title: None,
        description: None,
        fields: vec![ElicitationField {
            id: "question_0".into(),
            title: "Architecture".into(),
            description: None,
            required,
            secret: false,
            custom_answer_for: None,
            custom_answer_option: None,
            kind,
        }],
        tool_permission: None,
    }
}

fn paired_request(question_count: usize, multi_select: bool) -> ElicitationRequest {
    let mut fields = Vec::new();
    for index in 0..question_count {
        let id = format!("question_{index}");
        let options = vec![
            ElicitationOption {
                value: "alpha".into(),
                title: "Alpha".into(),
                description: Some("Choose alpha".into()),
                preview: None,
            },
            ElicitationOption {
                value: "beta".into(),
                title: "Beta".into(),
                description: Some("Choose beta".into()),
                preview: None,
            },
        ];
        fields.push(ElicitationField {
            id: id.clone(),
            title: format!("Question {}", index + 1),
            description: Some(format!("Prompt {}", index + 1)),
            required: false,
            secret: false,
            custom_answer_for: None,
            custom_answer_option: None,
            kind: if multi_select {
                ElicitationFieldKind::MultiSelect {
                    options,
                    default: Vec::new(),
                    min_items: None,
                    max_items: None,
                }
            } else {
                ElicitationFieldKind::SingleSelect {
                    options,
                    default: None,
                }
            },
        });
        fields.push(ElicitationField {
            id: format!("{id}__other"),
            title: "Other".into(),
            description: Some("Type your own answer instead of choosing an option above.".into()),
            required: false,
            secret: false,
            custom_answer_for: Some(id),
            custom_answer_option: None,
            kind: ElicitationFieldKind::Text {
                default: None,
                min_length: None,
                max_length: None,
                pattern: None,
                format: None,
            },
        });
    }
    ElicitationRequest {
        id: "ask-paired".into(),
        message: "Input requested".into(),
        title: None,
        description: None,
        fields,
        tool_permission: None,
    }
}

fn rendered(dialog: &ElicitationDialog) -> String {
    rendered_with_surfaces(dialog, &mut FrameSurfaces::new())
}

fn rendered_in_pane(dialog: &ElicitationDialog, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal
        .draw(|frame| {
            let area = frame.area();
            render_elicitation_in(frame, dialog, &mut FrameSurfaces::new(), area, true);
        })
        .expect("render question pane");
    terminal.backend().buffer().clone()
}

fn buffer_row(buffer: &Buffer, row: u16, start: u16, end: u16) -> String {
    (start..end)
        .map(|column| buffer[(column, row)].symbol())
        .collect()
}

fn buffer_text(buffer: &Buffer) -> String {
    crate::golden::buffer_lines(buffer).join("\n")
}

#[test]
fn an_upgrade_preserves_unsubmitted_form_answers() {
    let request = request(
        ElicitationFieldKind::Text {
            default: None,
            min_length: None,
            max_length: None,
            pattern: None,
            format: None,
        },
        true,
    );
    let mut dialog = ElicitationDialog::new(request.clone());
    let text = "Keep this answer 🦀 ".repeat(8_000);
    dialog.paste(&text);
    let encoded = serde_json::to_vec(&dialog.draft()).unwrap();
    assert!(encoded.len() > 64 * 1024);
    let draft = serde_json::from_slice(&encoded).unwrap();
    let mut restored = ElicitationDialog::from_draft(request, draft).unwrap();
    restored.focus_control(1);
    assert_eq!(
        restored.handle_key(KeyCode::Enter, KeyModifiers::NONE),
        Some(ElicitationResponse::Accept {
            content: BTreeMap::from([("question_0".into(), ElicitationValue::String(text))]),
        })
    );
}

#[test]
fn pane_resize_preserves_the_visible_word_inside_an_indented_paragraph() {
    for (indent, old_width, new_width) in [
        ("", 42, 62),
        ("             ", 33, 57),
        ("                       ", 57, 37),
        (
            "                                                                                ",
            42,
            62,
        ),
    ] {
        let message = format!(
            "{indent}{}",
            (0..200)
                .map(|n| format!("word{n:03}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let dialog = plan_review_message(&message);
        rendered_in_pane(&dialog, old_width, 18);
        dialog.scroll_message(8);
        let before = rendered_in_pane(&dialog, old_width, 18);
        let area = dialog.message_area.get().expect("message area");
        let before_row = buffer_row(&before, area.y, area.x, area.right());
        let anchor = before_row.split_whitespace().next().expect("visible word");
        let after = rendered_in_pane(&dialog, new_width, 18);
        let area = dialog.message_area.get().expect("resized message area");
        let after_row = buffer_row(&after, area.y, area.x, area.right());
        assert!(
            after_row.contains(anchor),
            "anchor {anchor:?} disappeared from top row {after_row:?}, indent length {}",
            indent.len()
        );
    }

    let message = (0..400)
        .map(|index| format!("word-{index:03}"))
        .collect::<Vec<_>>()
        .join(" ");
    let dialog = plan_review_message(&message);
    rendered_in_pane(&dialog, 42, 18);
    dialog.scroll_message(8);
    let anchor = dialog.message_anchor.get().expect("scroll anchor");
    for width in [34, 62, 42] {
        rendered_in_pane(&dialog, width, 18);
        assert_eq!(dialog.message_anchor.get(), Some(anchor));
    }

    let boundary_message = (0..400).map(|n| format!("w{n:04}")).collect::<String>();
    let dialog = plan_review_message(&boundary_message);
    rendered_in_pane(&dialog, 42, 18);
    dialog.scroll_message(8);
    let before = rendered_in_pane(&dialog, 42, 18);
    let area = dialog.message_area.get().unwrap();
    let before_row = buffer_row(&before, area.y, area.x, area.right());
    let after = rendered_in_pane(&dialog, 34, 18);
    let area = dialog.message_area.get().unwrap();
    let after_row = buffer_row(&after, area.y, area.x, area.right());
    assert!(
        after_row.starts_with(&before_row[..8]),
        "reading position moved from {before_row:?} to {after_row:?}"
    );
}
#[test]
fn six_row_plan_pane_keeps_text_and_page_down_actionable() {
    let message = (0..80)
        .map(|index| format!("plan-{index:03}"))
        .collect::<Vec<_>>()
        .join(" ");
    let mut dialog = plan_review_message(&message);
    let before = rendered_in_pane(&dialog, 60, 6);
    let before_text = (0..6)
        .map(|row| buffer_row(&before, row, 0, 60))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        before_text.contains("plan-000"),
        "plan text is hidden: {before_text}"
    );
    assert!(
        before_text.contains("Answer & Next"),
        "submit is hidden: {before_text}"
    );
    assert!(
        before_text.contains("PgUp/PgDn"),
        "the footer hints are hidden: {before_text}"
    );

    dialog.handle_key(KeyCode::PageDown, KeyModifiers::NONE);
    let after = rendered_in_pane(&dialog, 60, 6);
    let after_text = (0..6)
        .map(|row| buffer_row(&after, row, 0, 60))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !after_text.contains("plan-000"),
        "PageDown did not scroll: {after_text}"
    );
    assert!(
        after_text.contains("plan-"),
        "scrolled plan text is hidden: {after_text}"
    );
}

fn rendered_with_surfaces(dialog: &ElicitationDialog, surfaces: &mut FrameSurfaces) -> String {
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
    terminal
        .draw(|frame| {
            let area = frame.area();
            render_elicitation_in(frame, dialog, surfaces, area, true);
        })
        .expect("render elicitation");
    let buffer = terminal.backend().buffer();
    (buffer.area.y..buffer.area.bottom())
        .map(|y| {
            (buffer.area.x..buffer.area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn plan_review(line_count: usize) -> ElicitationDialog {
    plan_review_message(
        &(0..line_count)
            .map(|line| format!("plan-line-{line:02}"))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

fn plan_review_message(message: &str) -> ElicitationDialog {
    let mut request = request(
        ElicitationFieldKind::SingleSelect {
            options: vec![
                ElicitationOption {
                    value: "implement".into(),
                    title: "Implement".into(),
                    description: Some("Approve and continue".into()),
                    preview: None,
                },
                ElicitationOption {
                    value: "revise".into(),
                    title: "Revise".into(),
                    description: None,
                    preview: None,
                },
            ],
            default: Some("implement".into()),
        },
        true,
    );
    request.id = "plan-review-test".into();
    request.title = Some("Plan review".into());
    request.fields.push(ElicitationField {
        id: "feedback".into(),
        title: "Revision feedback".into(),
        description: Some("Describe what the agent should change".into()),
        required: false,
        secret: false,
        custom_answer_for: Some("question_0".into()),
        custom_answer_option: Some("revise".into()),
        kind: ElicitationFieldKind::Text {
            default: None,
            min_length: None,
            max_length: None,
            pattern: None,
            format: None,
        },
    });
    request.message = message.to_owned();
    ElicitationDialog::new(request)
}

/// The pane a plan-review dialog registered on its last frame.
fn message_pane(dialog: &ElicitationDialog) -> SurfaceFrame {
    let mut surfaces = FrameSurfaces::new();
    rendered_with_surfaces(dialog, &mut surfaces);
    *surfaces
        .surface(SurfaceId::ElicitationMessage)
        .expect("the message pane is registered")
}

fn range(start: (usize, u16), end: (usize, u16)) -> SelectionRange {
    SelectionRange {
        start: ContentPos::new(start.0, start.1),
        end: ContentPos::new(end.0, end.1),
    }
}

#[test]
fn plan_review_preserves_content_outside_scoped_bounds() {
    const WIDTH: u16 = 100;
    const HEIGHT: u16 = 30;
    let dialog = plan_review(20);
    let screen = Rect::new(0, 0, WIDTH, HEIGHT);
    let question_bounds = Rect::new(10, 5, 80, 20);
    let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).expect("terminal");

    terminal
        .draw(|frame| {
            let background = (0..HEIGHT)
                .map(|_| Line::raw("X".repeat(usize::from(WIDTH))))
                .collect::<Vec<_>>();
            frame.render_widget(Paragraph::new(background), frame.area());
            render_elicitation_in(
                frame,
                &dialog,
                &mut FrameSurfaces::new(),
                question_bounds,
                true,
            );
        })
        .expect("render plan review over background");

    let buffer = terminal.backend().buffer();
    for y in screen.y..screen.bottom() {
        for x in screen.x..screen.right() {
            let position = Position::new(x, y);
            if !question_bounds.contains(position) {
                assert_eq!(
                    buffer[(x, y)].symbol(),
                    "X",
                    "scoped question changed content outside ({x}, {y})"
                );
            }
        }
    }
    assert_eq!(buffer[(question_bounds.x, question_bounds.y)].symbol(), "╭");
    assert_eq!(
        buffer[(question_bounds.right() - 1, question_bounds.y)].symbol(),
        "╮"
    );
}

/// The point of the feature: a range over whole logical lines comes back
/// as the plan wrote them, without the newlines word wrap introduced, even
/// though most of those rows were scrolled out of the pane.
#[test]
fn copying_a_plan_range_past_the_viewport_returns_the_unwrapped_source_lines() {
    let paragraph = "This step is long enough that the plan pane wraps it over several rows, which is exactly the fugliness copying is meant to undo.";
    let message = (0..12)
        .map(|step| format!("Step {step}: {paragraph}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    let dialog = plan_review_message(&message);
    let pane = message_pane(&dialog);
    assert!(
        pane.total_rows > usize::from(pane.rect.height),
        "the fixture has to outgrow the pane"
    );

    let selection = range((0, 0), (pane.total_rows - 1, pane.rect.width - 1));

    assert_eq!(dialog.selection_text(&selection, pane.rect.width), message);
}

#[test]
fn partial_first_and_last_plan_lines_are_cut_at_the_selected_columns() {
    let dialog = plan_review_message("alpha beta gamma\n世界 wide row\nomega");
    // At this width the pane draws "alpha beta" / "gamma" / "世界 wide" /
    // "row" / "omega"; the wide graphemes take two cells each.
    let width = 10;

    assert_eq!(
        dialog.selection_text(&range((0, 6), (3, 2)), width),
        "beta gamma\n世界 wide row"
    );
}

#[test]
fn a_range_inside_one_wrapped_line_rejoins_the_rows_word_wrap_split() {
    let dialog = plan_review_message("alpha beta gamma\n世界 wide row\nomega");

    assert_eq!(
        dialog.selection_text(&range((0, 6), (1, 2)), 10),
        "beta gam"
    );
}
#[test]
fn mouse_wheel_scrolls_the_plan_without_moving_the_decision() {
    let mut dialog = plan_review(80);
    rendered(&dialog);
    let area = dialog.message_area.get().expect("rendered plan area");

    dialog.handle_mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: area.x,
        row: area.y,
        modifiers: KeyModifiers::NONE,
    });

    assert_eq!(dialog.message_scroll.get(), 3);
    assert_eq!(dialog.focus_index(), 0);
    assert!(rendered(&dialog).contains("plan-line-03"));
}
#[test]
fn leaving_revise_keeps_its_draft_out_of_the_answer() {
    let mut dialog = plan_review(4);
    dialog.handle_key(KeyCode::Down, KeyModifiers::NONE);
    dialog.paste("stale revision");
    dialog.handle_key(KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(
        dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
        Some(ElicitationResponse::Accept {
            content: BTreeMap::from([(
                "question_0".into(),
                ElicitationValue::String("implement".into())
            )])
        })
    );
}
fn append_elicitation_golden_state(
    output: &mut String,
    label: &str,
    buffer: &Buffer,
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
    output.push_str(&buffer_text(buffer));
    output.push('\n');
    for detail in details {
        writeln!(output, "{detail}").expect("write state detail");
    }
}

// Hard-won: #1265: async Codex questions must expose both suggested and custom answers.
// Hard-won: #1312: Claude's Other dropped the picks beside it and a required Codex question's own field.
#[test]
fn golden_elicitation_dialog() {
    let mut output = String::new();

    // Recorded ACP form from the Codex bridge's async-user-input regression.
    let billing_request = ElicitationRequest::from_acp_params(
        "async-billing-question",
        serde_json::from_str(include_str!("testdata/codex-async-user-input.json")).unwrap(),
    )
    .unwrap();
    let mut billing = ElicitationDialog::new(billing_request);
    let buffer = rendered_in_pane(&billing, 100, 20);
    append_elicitation_golden_state(
        &mut output,
        "Codex async question with a suggested answer",
        &buffer,
        &[],
    );
    billing.handle_key(KeyCode::Down, KeyModifiers::NONE);
    billing.handle_key(KeyCode::Down, KeyModifiers::NONE);
    billing.paste("Use a third AWS billing account");
    let buffer = rendered_in_pane(&billing, 100, 20);
    append_elicitation_golden_state(
        &mut output,
        "Codex async question with a custom answer",
        &buffer,
        &[],
    );
    billing.handle_key(KeyCode::Enter, KeyModifiers::NONE);
    billing.handle_key(KeyCode::Down, KeyModifiers::NONE);
    billing.handle_key(KeyCode::Enter, KeyModifiers::NONE);
    billing.paste("BedrockCI");
    let answer = billing
        .handle_key(KeyCode::Enter, KeyModifiers::NONE)
        .expect("the last answer submits the form");
    append_elicitation_golden_state(
        &mut output,
        "Codex answers as submitted and as the harness receives them",
        &rendered_in_pane(&billing, 100, 20),
        &[
            format!("submitted: {answer:?}"),
            format!(
                "harness: {:?}",
                billing.request.harness_response(answer.clone())
            ),
        ],
    );

    // Recorded ACP form from claude-agent-acp 0.90.0 for an Air client
    // (`askUserQuestionsToCreateRequest`): a single-select and a
    // multi-select question, each with its "Other" companion.
    let mut claude = ElicitationDialog::new(
        ElicitationRequest::from_acp_params(
            "claude-questions",
            serde_json::from_str(include_str!("testdata/claude-ask-user-question.json")).unwrap(),
        )
        .unwrap(),
    );
    let buffer = rendered_in_pane(&claude, 100, 20);
    append_elicitation_golden_state(
        &mut output,
        "Claude optional question starts with nothing picked",
        &buffer,
        &[],
    );
    // Pick first, then write: arrows to the text box under the choices pass
    // Memcached without picking it, and the pick stays Redis.
    claude.handle_key(KeyCode::Char(' '), KeyModifiers::NONE);
    claude.handle_key(KeyCode::Down, KeyModifiers::NONE);
    claude.handle_key(KeyCode::Down, KeyModifiers::NONE);
    claude.paste("Keep TTLs short");
    let buffer = rendered_in_pane(&claude, 100, 20);
    append_elicitation_golden_state(
        &mut output,
        "Claude pick kept while writing Other",
        &buffer,
        &[],
    );
    claude.handle_key(KeyCode::Enter, KeyModifiers::NONE);
    claude.handle_key(KeyCode::Char(' '), KeyModifiers::NONE);
    claude.handle_key(KeyCode::Down, KeyModifiers::NONE);
    claude.handle_key(KeyCode::Char(' '), KeyModifiers::NONE);
    claude.handle_key(KeyCode::End, KeyModifiers::NONE);
    claude.paste("freebsd");
    let buffer = rendered_in_pane(&claude, 100, 20);
    append_elicitation_golden_state(
        &mut output,
        "Claude multi-select picks with Other text",
        &buffer,
        &[],
    );
    let answer = claude
        .handle_key(KeyCode::Enter, KeyModifiers::NONE)
        .expect("the last answer submits the form");
    append_elicitation_golden_state(
        &mut output,
        "Claude answers as submitted and as the harness receives them",
        &rendered_in_pane(&claude, 100, 20),
        &[
            format!("submitted: {answer:?}"),
            format!(
                "harness: {:?}",
                claude.request.harness_response(answer.clone())
            ),
        ],
    );
    // With nothing picked, the text alone is the whole answer, also when the
    // arrows pass every option on the way to it.
    let mut own = ElicitationDialog::new(claude.request.clone());
    own.handle_key(KeyCode::Down, KeyModifiers::NONE);
    own.handle_key(KeyCode::Down, KeyModifiers::NONE);
    for character in "Valkey".chars() {
        own.handle_key(KeyCode::Char(character), KeyModifiers::NONE);
    }
    let buffer = rendered_in_pane(&own, 100, 20);
    own.handle_key(KeyCode::Enter, KeyModifiers::NONE);
    own.handle_key(KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        own.confirming_unanswered,
        "the targets question is unanswered"
    );
    own.handle_key(KeyCode::Tab, KeyModifiers::NONE);
    let answer = own
        .handle_key(KeyCode::Char(' '), KeyModifiers::NONE)
        .expect("submitting with an unanswered question still answers");
    append_elicitation_golden_state(
        &mut output,
        "Claude Other text reached with arrows picks nothing on the way",
        &buffer,
        &[format!("submitted: {answer:?}")],
    );

    // A click or Space toggles an option: it picks it, replacing any other
    // pick, and takes back the option already picked. A mouse-only answer
    // can therefore still end with nothing picked.
    let page_answer = |dialog: &ElicitationDialog| format!("{:?}", dialog.question_content(0));
    // Clicks on a fixed clock, each well past the double-click interval.
    let start = std::time::Instant::now();
    let later = |clicks: u32| start + crate::components::DOUBLE_CLICK_INTERVAL * 3 * clicks;
    let mut toggles = ElicitationDialog::new(claude.request.clone());
    let mut steps = Vec::new();
    for (click, (label, step)) in [
        ("click Redis", "Redis"),
        ("click Memcached", "Memcached"),
        ("click Memcached again, later", "Memcached"),
    ]
    .into_iter()
    .enumerate()
    {
        click_question_button_at(&mut toggles, step, later(click as u32));
        steps.push(format!("{label}: {}", page_answer(&toggles)));
    }
    toggles.handle_key(KeyCode::Up, KeyModifiers::NONE);
    toggles.handle_key(KeyCode::Char(' '), KeyModifiers::NONE);
    steps.push(format!("Space on Redis: {}", page_answer(&toggles)));
    toggles.handle_key(KeyCode::Char(' '), KeyModifiers::NONE);
    steps.push(format!("Space on Redis again: {}", page_answer(&toggles)));
    click_question_button_at(&mut toggles, "Other", later(3));
    toggles.paste("Valkey");
    steps.push(format!(
        "click Other, write Valkey: {}",
        page_answer(&toggles)
    ));
    append_elicitation_golden_state(
        &mut output,
        "Claude clicks and Space toggle the pick",
        &rendered_in_pane(&toggles, 100, 20),
        &steps,
    );

    // A double-click on an option is an activation, as on any other list:
    // the option ends picked and the question is answered. A real
    // double-click lands twice on one screen cell, and the first click moves
    // the cursor, which reflows the rows below it, so the second click lands
    // on whatever row now occupies that cell. Starting from the default
    // cursor on Redis, that is Other on the recorded form and Dragonfly once
    // a third option follows Memcached.
    let mut with_dragonfly: serde_json::Value =
        serde_json::from_str(include_str!("testdata/claude-ask-user-question.json")).unwrap();
    with_dragonfly["requestedSchema"]["properties"]["question_0"]["oneOf"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "const": "Dragonfly",
            "title": "Dragonfly",
            "description": "Redis-compatible, multi-threaded"
        }));
    let mut steps = Vec::new();
    let mut double = ElicitationDialog::new(claude.request.clone());
    for (label, request) in [
        ("recorded form", claude.request.clone()),
        (
            "with Dragonfly after Memcached",
            ElicitationRequest::from_acp_params("claude-questions", with_dragonfly).unwrap(),
        ),
    ] {
        double = ElicitationDialog::new(request);
        let (column, row) = label_cell(&double, "Memcached");
        let first = start + crate::components::DOUBLE_CLICK_INTERVAL * 3;
        click_cell_at(&mut double, column, row, first);
        let redrawn = rendered_in_pane(&double, 110, 22);
        let under_cell = buffer_row(&redrawn, row, 0, redrawn.area.width)
            .trim_matches(|character: char| character == '│' || character.is_whitespace())
            .to_owned();
        let response = click_cell_at(
            &mut double,
            column,
            row,
            first + crate::components::DOUBLE_CLICK_INTERVAL / 2,
        );
        steps.push(format!(
            "{label}: second click lands on {under_cell:?}; {}, answered={}, now on question {}, response={response:?}",
            page_answer(&double),
            double.confirmed.contains(&0),
            double.current_question + 1,
        ));
    }
    append_elicitation_golden_state(
        &mut output,
        "Claude double-click picks the option and answers",
        &rendered_in_pane(&double, 100, 20),
        &steps,
    );

    // Enter answers with what is already there. It takes the option under
    // the cursor only when the question would otherwise be empty.
    let mut empty = ElicitationDialog::new(claude.request.clone());
    empty.handle_key(KeyCode::Down, KeyModifiers::NONE);
    empty.handle_key(KeyCode::Enter, KeyModifiers::NONE);
    let mut written = ElicitationDialog::new(claude.request.clone());
    written.handle_key(KeyCode::Down, KeyModifiers::NONE);
    written.handle_key(KeyCode::Down, KeyModifiers::NONE);
    written.paste("Valkey");
    written.handle_key(KeyCode::Up, KeyModifiers::NONE);
    let buffer = rendered_in_pane(&written, 100, 20);
    written.handle_key(KeyCode::Enter, KeyModifiers::NONE);
    append_elicitation_golden_state(
        &mut output,
        "Claude Enter on an option answers with what is there",
        &buffer,
        &[
            format!(
                "Enter on Memcached, nothing picked, Other blank: {}",
                page_answer(&empty)
            ),
            format!(
                "Enter on Memcached, nothing picked, Other holds Valkey: {}",
                page_answer(&written)
            ),
        ],
    );

    // Tool permissions share the three form actions; rejection is chosen
    // from their permission options. Preserve the harness response semantics
    // for one-time rejection and persistent network rules.
    for (label, id, message, options, skip) in [
        (
            "Claude permission form uses the same three actions",
            "tool-permission-1",
            "Claude Code requests permission:\nRun cargo test",
            serde_json::json!([
                {"const": "allow-once", "title": "Yes"},
                {"const": "allow-with-updates", "title": "Yes, and don't ask again for `cargo test` commands"},
                {"const": "reject", "title": "No"}
            ]),
            true,
        ),
        (
            "persistent permission form uses the same three actions",
            "tool-permission-2",
            "Codex requests permission:\nReach crates.io",
            serde_json::json!([
                {"const": "network_policy_amendment:0", "title": "Yes, and allow this host in the future"},
                {"const": "network_policy_amendment:1", "title": "No, and block this host in the future"}
            ]),
            false,
        ),
    ] {
        let mut request = ElicitationRequest::from_acp_params(
            id,
            serde_json::json!({
                "mode": "form", "sessionId": "session-1",
                "message": message,
                "requestedSchema": {"type": "object", "required": ["choice"], "properties": {
                    "choice": {"type": "string", "title": "Permission", "oneOf": options}
                }}
            }),
        )
        .unwrap();
        request.tool_permission = Some(mj_core::elicitation::ToolPermissionForm { skip });
        let dialog = ElicitationDialog::new(request);
        append_elicitation_golden_state(
            &mut output,
            label,
            &rendered_in_pane(&dialog, 100, 14),
            &[format!(
                "skip: {:?}",
                dialog
                    .request
                    .harness_response(ElicitationResponse::Decline)
            )],
        );
    }

    let mut smallest = ElicitationDialog::new(paired_request(1, true));
    smallest.handle_key(KeyCode::End, KeyModifiers::NONE);
    smallest.paste("custom draft");
    let buffer = rendered_in_pane(&smallest, 60, 6);
    append_elicitation_golden_state(
        &mut output,
        "small question pane keeps the focused custom answer and actions",
        &buffer,
        &[format!("focused value={:?}", smallest.values)],
    );

    let mut plan = plan_review(80);
    let before = rendered_in_pane(&plan, 100, 30);
    append_elicitation_golden_state(
        &mut output,
        "plan review before scrolling",
        &before,
        &[format!("focus index={}", plan.focus_index())],
    );
    for _ in 0..10 {
        plan.handle_key(KeyCode::PageDown, KeyModifiers::NONE);
        let _ = rendered_in_pane(&plan, 100, 30);
    }
    let after = rendered_in_pane(&plan, 100, 30);
    append_elicitation_golden_state(
        &mut output,
        "plan review after paging to the end",
        &after,
        &[format!("focus index={}", plan.focus_index())],
    );

    for (label, click_title) in [
        ("boolean checkbox click", false),
        ("boolean field title click", true),
    ] {
        let mut dialog = ElicitationDialog::new(request(
            ElicitationFieldKind::Boolean {
                default: Some(false),
            },
            false,
        ));
        let initial = rendered_in_pane(&dialog, 100, 30);
        let point = if click_title {
            let title = &dialog.request.fields[0].title;
            (0..initial.area.height)
                .find_map(|row| {
                    let text = (0..initial.area.width)
                        .map(|column| initial[(column, row)].symbol())
                        .collect::<String>();
                    text.find(title)
                        .map(|byte| (text[..byte].chars().count() as u16, row))
                })
                .expect("boolean field title is rendered")
        } else {
            (0..initial.area.width)
                .flat_map(|column| (0..initial.area.height).map(move |row| (column, row)))
                .find(|&(column, row)| initial[(column, row)].symbol() == "☐")
                .expect("boolean checkbox is rendered")
        };
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            dialog.handle_mouse(MouseEvent {
                kind,
                column: point.0,
                row: point.1,
                modifiers: KeyModifiers::NONE,
            });
        }
        let buffer = rendered_in_pane(&dialog, 100, 30);
        append_elicitation_golden_state(
            &mut output,
            label,
            &buffer,
            &[format!("field value={:?}", dialog.values[0])],
        );
    }

    let mut revise = plan_review(4);
    let buffer = rendered_in_pane(&revise, 100, 30);
    append_elicitation_golden_state(
        &mut output,
        "plan review implement option selected",
        &buffer,
        &[],
    );
    revise.handle_key(KeyCode::Down, KeyModifiers::NONE);
    let buffer = rendered_in_pane(&revise, 100, 30);
    append_elicitation_golden_state(
        &mut output,
        "revise option exposes inline feedback",
        &buffer,
        &[],
    );
    revise.paste("add a regression test");
    let action = revise.handle_key(KeyCode::Enter, KeyModifiers::NONE);
    append_elicitation_golden_state(
        &mut output,
        "revise submits its feedback with the selected action",
        &rendered_in_pane(&revise, 100, 30),
        &[format!("action: {action:?}")],
    );

    let mut paired = ElicitationDialog::new(paired_request(3, false));
    let first = rendered_in_pane(&paired, 100, 30);
    append_elicitation_golden_state(
        &mut output,
        "paired custom answer starts on question one of three",
        &first,
        &[format!("display fields={}", paired.display_fields.len())],
    );
    paired.handle_key(KeyCode::Enter, KeyModifiers::NONE);
    let second = rendered_in_pane(&paired, 100, 30);
    append_elicitation_golden_state(
        &mut output,
        "paired custom answer advances to question two of three",
        &second,
        &[format!("display fields={}", paired.display_fields.len())],
    );

    paired.handle_key(KeyCode::Enter, KeyModifiers::NONE);
    append_elicitation_golden_state(
        &mut output,
        "last question keeps the same three actions",
        &rendered_in_pane(&paired, 100, 20),
        &[],
    );
    click_question_button(&mut paired, "Previous");
    append_elicitation_golden_state(
        &mut output,
        "previous returns to the confirmed answer",
        &rendered_in_pane(&paired, 100, 20),
        &[],
    );

    let mut partial = ElicitationDialog::new(paired_request(1, true));
    partial.handle_key(KeyCode::Enter, KeyModifiers::NONE);
    append_elicitation_golden_state(
        &mut output,
        "unanswered warning keeps the same three actions",
        &rendered_in_pane(&partial, 100, 20),
        &[],
    );
    let response = click_question_button(&mut partial, "Cancel");
    append_elicitation_golden_state(
        &mut output,
        "cancel remains available from the unanswered warning",
        &rendered_in_pane(&partial, 100, 20),
        &[format!("action: {response:?}")],
    );

    mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), "elicitation-dialog", &output);
}

#[test]
fn custom_answer_uses_the_adapter_field_instead_of_the_stale_selection() {
    let mut dialog = ElicitationDialog::new(paired_request(1, false));
    dialog.handle_key(KeyCode::Down, KeyModifiers::NONE);
    dialog.handle_key(KeyCode::Down, KeyModifiers::NONE);
    for character in "custom answer".chars() {
        dialog.handle_key(KeyCode::Char(character), KeyModifiers::NONE);
    }
    assert_eq!(
        dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
        Some(ElicitationResponse::Accept {
            content: BTreeMap::from([(
                "question_0__other".into(),
                ElicitationValue::String("custom answer".into())
            )])
        })
    );
}

#[test]
fn choosing_an_option_after_typing_other_omits_the_custom_draft() {
    let mut dialog = ElicitationDialog::new(paired_request(1, false));
    dialog.handle_key(KeyCode::End, KeyModifiers::NONE);
    dialog.paste("custom draft");
    dialog.handle_key(KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(
        dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
        Some(ElicitationResponse::Accept {
            content: BTreeMap::from([(
                "question_0".into(),
                ElicitationValue::String("beta".into())
            )])
        })
    );
}

#[test]
fn toggling_a_multi_select_option_deactivates_other() {
    let mut dialog = ElicitationDialog::new(paired_request(1, true));
    dialog.handle_key(KeyCode::End, KeyModifiers::NONE);
    dialog.paste("custom draft");
    dialog.handle_key(KeyCode::Up, KeyModifiers::NONE);
    dialog.handle_key(KeyCode::Char(' '), KeyModifiers::NONE);
    assert_eq!(
        dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
        Some(ElicitationResponse::Accept {
            content: BTreeMap::from([(
                "question_0".into(),
                ElicitationValue::StringArray(vec!["beta".into()])
            )])
        })
    );
}

#[test]
fn dangling_custom_metadata_remains_a_standalone_page() {
    let mut request = paired_request(1, false);
    request.fields[1].custom_answer_for = Some("missing".into());
    let mut dialog = ElicitationDialog::new(request);

    assert_eq!(dialog.display_fields.len(), 2);
    dialog.handle_key(KeyCode::Right, KeyModifiers::ALT);
    assert!(rendered(&dialog).contains("2/2"));
    assert!(rendered(&dialog).contains("Other"));
}

fn component_form() -> ElicitationRequest {
    ElicitationRequest {
        id: "ask-component".into(),
        message: "Component form: edit the label and choose the options.".into(),
        title: None,
        description: None,
        fields: vec![
            ElicitationField {
                id: "label".into(),
                title: "Label".into(),
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
            },
            ElicitationField {
                id: "enabled".into(),
                title: "Enabled".into(),
                description: None,
                required: false,
                secret: false,
                custom_answer_for: None,
                custom_answer_option: None,
                kind: ElicitationFieldKind::Boolean {
                    default: Some(false),
                },
            },
            ElicitationField {
                id: "choice".into(),
                title: "Choice".into(),
                description: None,
                required: false,
                secret: false,
                custom_answer_for: None,
                custom_answer_option: None,
                kind: ElicitationFieldKind::SingleSelect {
                    options: vec![
                        ElicitationOption {
                            value: "one".into(),
                            title: "One".into(),
                            description: None,
                            preview: None,
                        },
                        ElicitationOption {
                            value: "two".into(),
                            title: "Two".into(),
                            description: None,
                            preview: None,
                        },
                    ],
                    default: None,
                },
            },
        ],
        tool_permission: None,
    }
}

// Hard-won: 9651f8c: compact forms scrolled the focused field title away from its control
#[test]
fn compact_question_pane_shows_the_focused_field_title_with_its_control() {
    let mut dialog = ElicitationDialog::new(component_form());
    dialog.handle_key(KeyCode::Right, KeyModifiers::ALT);
    assert_eq!(dialog.focus_index(), 1);

    let width = 78;
    for height in [dialog.natural_height(width), 10, 8, 6, 5] {
        let text = buffer_text(&rendered_in_pane(&dialog, width, height));
        assert!(
            text.contains("Enabled"),
            "field title missing at height {height}:\n{text}"
        );
        assert!(
            text.contains("☐ No"),
            "boolean control missing at height {height}:\n{text}"
        );
        assert!(text.contains("Answer & Next"), "actions missing:\n{text}");
    }
}

#[test]
fn compact_question_keeps_validation_errors_and_the_labeled_control_visible() {
    let mut dialog = ElicitationDialog::new(request(
        ElicitationFieldKind::Text {
            default: None,
            min_length: None,
            max_length: None,
            pattern: None,
            format: None,
        },
        true,
    ));
    dialog.focus_control(1);
    assert_eq!(dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE), None);
    let text = buffer_text(&rendered_in_pane(&dialog, 78, 5));
    assert!(
        text.contains("Architecture") && text.contains("1/1"),
        "field title missing:\n{text}"
    );
    assert!(
        text.contains("Architecture is required"),
        "error missing:\n{text}"
    );
    assert!(text.contains("Answer & Next"), "actions missing:\n{text}");
}

// Hard-won: 9651f8c: scrolling an option list hid the title identifying its field
#[test]
fn a_scrolled_option_list_keeps_showing_the_field_it_answers() {
    let options = (0..12)
        .map(|index| ElicitationOption {
            value: format!("option-{index}"),
            title: format!("Option {index}"),
            description: None,
            preview: None,
        })
        .collect::<Vec<_>>();
    let mut dialog = ElicitationDialog::new(request(
        ElicitationFieldKind::SingleSelect {
            options,
            default: None,
        },
        false,
    ));
    for _ in 0..11 {
        dialog.handle_key(KeyCode::Down, KeyModifiers::NONE);
    }
    let text = buffer_text(&rendered_in_pane(&dialog, 78, 12));
    assert!(
        text.contains("Architecture"),
        "field title missing while the last option is focused:\n{text}"
    );
    assert!(
        text.contains("Option 11"),
        "focused option missing:\n{text}"
    );
}

#[test]
fn answering_advances_one_question_and_only_sends_confirmed_answers() {
    let mut dialog = ElicitationDialog::new(paired_request(3, false));
    assert_eq!(dialog.unanswered().count(), 3);
    assert_eq!(dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE), None);
    assert_eq!(dialog.current_question, 1);
    assert_eq!(dialog.unanswered().count(), 2);
    dialog.handle_key(KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE), None);
    assert_eq!(dialog.current_question, 2);
    assert_eq!(
        dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
        Some(ElicitationResponse::Accept {
            content: BTreeMap::from([
                (
                    "question_0".into(),
                    ElicitationValue::String("alpha".into())
                ),
                ("question_1".into(), ElicitationValue::String("beta".into())),
                (
                    "question_2".into(),
                    ElicitationValue::String("alpha".into())
                ),
            ])
        })
    );
}

#[test]
fn navigating_past_defaults_requires_a_separate_deliberate_partial_submission() {
    let mut dialog = ElicitationDialog::new(paired_request(3, false));
    dialog.handle_key(KeyCode::Right, KeyModifiers::ALT);
    dialog.handle_key(KeyCode::Right, KeyModifiers::ALT);
    assert_eq!(dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE), None);
    let warning = rendered(&dialog);
    assert!(warning.contains("2 unanswered"));
    assert!(warning.contains("Question 1"));
    assert!(warning.contains("Question 2"));
    // Enter on the default warning action goes back, never submits.
    assert_eq!(dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE), None);
    assert_eq!(dialog.current_question, 0);
    assert!(!dialog.confirming_unanswered);
    dialog.handle_key(KeyCode::Right, KeyModifiers::ALT);
    dialog.handle_key(KeyCode::Right, KeyModifiers::ALT);
    assert_eq!(dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE), None);
    dialog.handle_key(KeyCode::BackTab, KeyModifiers::SHIFT);
    assert_eq!(
        dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
        Some(ElicitationResponse::Accept {
            content: BTreeMap::from([(
                "question_2".into(),
                ElicitationValue::String("alpha".into())
            )])
        })
    );
}

#[test]
fn escape_from_the_warning_returns_to_the_first_unanswered_question() {
    let mut dialog = ElicitationDialog::new(paired_request(2, false));
    dialog.handle_key(KeyCode::Right, KeyModifiers::ALT);
    dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(dialog.handle_key(KeyCode::Esc, KeyModifiers::NONE), None);
    assert_eq!(dialog.current_question, 0);
    assert!(!dialog.confirming_unanswered);
}

#[test]
fn editing_a_confirmed_answer_requires_confirmation_again() {
    let mut dialog = ElicitationDialog::new(paired_request(2, false));
    dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE);
    dialog.handle_key(KeyCode::Left, KeyModifiers::ALT);
    assert_eq!(dialog.unanswered().count(), 1);
    dialog.handle_key(KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(dialog.unanswered().count(), 2);
    dialog.handle_key(KeyCode::Right, KeyModifiers::ALT);
    assert_eq!(dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE), None);
    assert!(dialog.confirming_unanswered);
}

#[test]
fn required_unconfirmed_defaults_return_to_the_required_question() {
    let mut request = paired_request(2, false);
    request.fields[0].required = true;
    let mut dialog = ElicitationDialog::new(request);
    dialog.handle_key(KeyCode::Right, KeyModifiers::ALT);
    assert_eq!(dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE), None);
    assert_eq!(dialog.current_question, 0);
    assert_eq!(dialog.error.as_deref(), Some("Question 1 is required"));
    assert!(!dialog.confirming_unanswered);
}

#[test]
fn false_is_an_answer_but_optional_whitespace_is_not() {
    let mut dialog = ElicitationDialog::new(request(
        ElicitationFieldKind::Boolean { default: None },
        true,
    ));
    assert!(rendered(&dialog).contains("Space toggle"));
    assert_eq!(
        dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
        Some(ElicitationResponse::Accept {
            content: BTreeMap::from([("question_0".into(), ElicitationValue::Boolean(false))])
        })
    );
    let mut request = paired_request(1, false);
    request.fields = vec![request.fields[1].clone()];
    let mut dialog = ElicitationDialog::new(request);
    dialog.paste("   ");
    assert_eq!(dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE), None);
    assert!(dialog.confirming_unanswered);
}

#[test]
fn empty_forms_still_submit_and_button_focus_keeps_the_question_visible() {
    let mut empty = paired_request(0, false);
    empty.message = "Continue?".into();
    let mut dialog = ElicitationDialog::new(empty);
    assert_eq!(
        dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
        Some(ElicitationResponse::Accept {
            content: BTreeMap::new()
        })
    );
    let mut dialog = ElicitationDialog::new(paired_request(2, false));
    dialog.handle_key(KeyCode::Tab, KeyModifiers::NONE);
    let screen = rendered(&dialog);
    assert!(screen.contains("Question 1"));
    assert!(screen.contains("Alpha"));
    assert_eq!(dialog.current_question, 0);
}

#[test]
fn upgrade_drafts_preserve_confirmation_and_old_drafts_start_unconfirmed() {
    let request = paired_request(3, false);
    let mut dialog = ElicitationDialog::new(request.clone());
    dialog.handle_key(KeyCode::End, KeyModifiers::NONE);
    dialog.paste("custom text");
    dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE);
    let encoded = serde_json::to_value(dialog.draft()).unwrap();
    let restored = ElicitationDialog::from_draft(
        request.clone(),
        serde_json::from_value(encoded.clone()).unwrap(),
    )
    .unwrap();
    assert_eq!(restored.current_question, 1);
    assert_eq!(restored.confirmed, BTreeSet::from([0]));
    let mut old = encoded;
    old.as_object_mut().unwrap().remove("confirmed");
    old.as_object_mut().unwrap().remove("current_question");
    let mut restored =
        ElicitationDialog::from_draft(request, serde_json::from_value(old).unwrap()).unwrap();
    assert_eq!(restored.unanswered().count(), 3);
    restored.handle_key(KeyCode::Left, KeyModifiers::ALT);
    assert!(rendered(&restored).contains("custom text"));
}

fn click_question_button(
    dialog: &mut ElicitationDialog,
    label: &str,
) -> Option<ElicitationResponse> {
    click_question_button_at(dialog, label, std::time::Instant::now())
}

/// Clicks `label` at an explicit time, so two clicks are a double-click
/// exactly when the test places them within the interval.
fn click_question_button_at(
    dialog: &mut ElicitationDialog,
    label: &str,
    now: std::time::Instant,
) -> Option<ElicitationResponse> {
    let (column, row) = label_cell(dialog, label);
    click_cell_at(dialog, column, row, now)
}

/// The screen cell where `label` starts once the dialog is drawn.
fn label_cell(dialog: &ElicitationDialog, label: &str) -> (u16, u16) {
    let buffer = rendered_in_pane(dialog, 110, 22);
    (0..buffer.area.height)
        .find_map(|row| {
            let line = buffer_row(&buffer, row, 0, buffer.area.width);
            line.find(label)
                .map(|byte| (line[..byte].chars().count() as u16, row))
        })
        .expect("label rendered")
}

/// Presses and releases the left button on one screen cell, as a real
/// click does, whatever is drawn there.
fn click_cell_at(
    dialog: &mut ElicitationDialog,
    column: u16,
    row: u16,
    now: std::time::Instant,
) -> Option<ElicitationResponse> {
    let mut response = None;
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        response = dialog.handle_mouse_at(
            MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            },
            now,
        );
    }
    response
}

#[test]
fn mouse_navigation_and_submission_share_the_keyboard_confirmation_rules() {
    let mut dialog = ElicitationDialog::new(paired_request(2, false));
    dialog.handle_key(KeyCode::End, KeyModifiers::NONE);
    dialog.paste(" ");
    assert_eq!(click_question_button(&mut dialog, "Answer & Next"), None);
    assert_eq!(click_question_button(&mut dialog, "Answer & Next"), None);
    assert!(dialog.confirming_unanswered);
    assert_eq!(
        click_question_button(&mut dialog, "Answer & Next"),
        Some(ElicitationResponse::Accept {
            content: BTreeMap::from([(
                "question_1".into(),
                ElicitationValue::String("alpha".into())
            )])
        })
    );
}

#[test]
fn multiline_question_text_wraps_scrolls_and_survives_resize() {
    let mut request = paired_request(1, false);
    request.fields[0].title = format!(
        "How should we handle JPEG support given that our existing linearization differs?\n{}\nFinal authored line",
        (0..100)
            .map(|index| format!("detail{index:03}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let mut dialog = ElicitationDialog::new(request);
    let first = buffer_text(&rendered_in_pane(&dialog, 48, 16));
    assert!(first.contains("How should we handle JPEG"));
    assert!(first.contains("existing linearization"));
    assert!(first.contains("Answer & Next"));
    dialog.handle_key(KeyCode::PageDown, KeyModifiers::NONE);
    let before = rendered_in_pane(&dialog, 48, 16);
    let area = dialog.message_area().unwrap();
    let top = buffer_row(&before, area.y, area.x, area.right());
    let anchor = top.split_whitespace().next().unwrap();
    let after = rendered_in_pane(&dialog, 64, 16);
    let area = dialog.message_area().unwrap();
    assert!(buffer_row(&after, area.y, area.x, area.right()).contains(anchor));
    dialog.scroll_message(isize::MAX);
    let last = buffer_text(&rendered_in_pane(&dialog, 48, 16));
    assert!(last.contains("Final authored line"), "{last}");
    assert!(last.contains("Answer & Next"));
}

#[test]
fn warning_buttons_do_not_edit_the_custom_answer_behind_them() {
    let mut dialog = ElicitationDialog::new(paired_request(2, false));
    dialog.handle_key(KeyCode::Right, KeyModifiers::ALT);
    dialog.handle_key(KeyCode::End, KeyModifiers::NONE);
    dialog.paste("custom answer");
    assert_eq!(dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE), None);
    assert!(dialog.confirming_unanswered);
    dialog.paste("accidental paste");
    dialog.handle_key(KeyCode::Char('x'), KeyModifiers::NONE);
    dialog.handle_key(KeyCode::BackTab, KeyModifiers::SHIFT);
    assert_eq!(
        dialog.handle_key(KeyCode::Char(' '), KeyModifiers::NONE),
        Some(ElicitationResponse::Accept {
            content: BTreeMap::from([(
                "question_1__other".into(),
                ElicitationValue::String("custom answer".into())
            )])
        })
    );
}
