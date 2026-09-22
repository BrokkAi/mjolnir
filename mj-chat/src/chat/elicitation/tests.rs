use super::*;
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use mj_core::elicitation::ElicitationOption;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

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
    (buffer.area.y..buffer.area.bottom())
        .map(|row| buffer_row(buffer, row, buffer.area.x, buffer.area.right()))
        .collect::<Vec<_>>()
        .join("\n")
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
}

#[test]
fn smallest_question_pane_keeps_other_and_its_draft_visible() {
    let mut dialog = ElicitationDialog::new(paired_request(1, true));
    dialog.handle_key(KeyCode::End, KeyModifiers::NONE);
    dialog.paste("custom draft");
    let buffer = rendered_in_pane(&dialog, 60, 6);
    let text = (0..6)
        .map(|row| buffer_row(&buffer, row, 0, 60))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("custom draft"),
        "focused custom input is hidden: {text}"
    );
    assert!(text.contains("Submit"), "submit is hidden: {text}");
    assert!(
        text.contains("Tab fields/buttons"),
        "the footer hints are hidden: {text}"
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
        before_text.contains("Submit"),
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

#[test]
fn repeated_resizes_retain_one_logical_message_anchor() {
    let message = (0..400)
        .map(|index| format!("word-{index:03}"))
        .collect::<Vec<_>>()
        .join(" ");
    let dialog = plan_review_message(&message);
    rendered_in_pane(&dialog, 42, 18);
    dialog.scroll_message(8);
    let anchor = dialog.message_anchor.get().expect("scroll anchor");
    rendered_in_pane(&dialog, 34, 18);
    assert_eq!(dialog.message_anchor.get(), Some(anchor));
    rendered_in_pane(&dialog, 62, 18);
    assert_eq!(dialog.message_anchor.get(), Some(anchor));
    rendered_in_pane(&dialog, 42, 18);
    assert_eq!(dialog.message_anchor.get(), Some(anchor));
}

#[test]
fn pane_resize_keeps_a_word_at_an_exact_wrap_boundary() {
    let message = (0..400).map(|n| format!("w{n:04}")).collect::<String>();
    let dialog = plan_review_message(&message);
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

/// The extractor maps a selection through per-line row counts, so those
/// counts have to add up to what the pane's own paragraph reports.
#[test]
fn wrapped_row_counts_of_source_lines_sum_to_the_paragraphs_line_count() {
    let message = concat!(
        "a short line\n",
        "\n",
        "a considerably longer line that the plan pane has to wrap over several rows ",
        "before it finally runs out of words to place\n",
        "tiny\n",
        "\n",
        "\n",
        "another long one, long enough that it also wraps more than once at any of ",
        "these widths"
    );

    for width in [17u16, 33, 80] {
        let composed = message
            .split('\n')
            .map(|line| wrapped_row_count(line, width))
            .sum::<usize>();
        let whole = Paragraph::new(message)
            .wrap(Wrap { trim: true })
            .line_count(width);
        assert_eq!(
            composed, whole,
            "per-line rows must compose at width {width}"
        );
    }
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
fn plan_review_gives_unused_form_rows_to_the_plan_and_scrolls_to_its_end() {
    let mut dialog = plan_review(80);

    let first = rendered(&dialog);
    assert!(first.contains("plan-line-12"));
    assert!(!first.contains("plan-line-79"));
    assert!(first.contains("PgUp/PgDn or wheel scroll"));

    for _ in 0..10 {
        dialog.handle_key(KeyCode::PageDown, KeyModifiers::NONE);
        rendered(&dialog);
    }
    let last = rendered(&dialog);
    assert!(last.contains("plan-line-79"));
    assert_eq!(dialog.focus_index(), 0);
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
fn mouse_click_toggles_the_focused_boolean_through_the_form() {
    let mut dialog = ElicitationDialog::new(request(
        ElicitationFieldKind::Boolean {
            default: Some(false),
        },
        false,
    ));
    let buffer = rendered_in_pane(&dialog, 100, 30);
    let point = (0..100)
        .flat_map(|column| (0..30).map(move |row| (column, row)))
        .find(|&(column, row)| buffer[(column, row)].symbol() == "☐")
        .expect("boolean control has a hitbox");
    let mouse = |kind| MouseEvent {
        kind,
        column: point.0,
        row: point.1,
        modifiers: KeyModifiers::NONE,
    };
    dialog.handle_mouse(mouse(MouseEventKind::Down(
        crossterm::event::MouseButton::Left,
    )));
    dialog.handle_mouse(mouse(MouseEventKind::Up(
        crossterm::event::MouseButton::Left,
    )));
    assert!(matches!(dialog.values[0], FieldValue::Boolean(true)));
}

#[test]
fn clicking_the_boolean_field_title_toggles_it() {
    let mut dialog = ElicitationDialog::new(request(
        ElicitationFieldKind::Boolean {
            default: Some(false),
        },
        false,
    ));
    let buffer = rendered_in_pane(&dialog, 100, 30);
    let title = &dialog.request.fields[0].title;
    let (column, row) = (0..buffer.area.height)
        .find_map(|row| {
            let text = (0..buffer.area.width)
                .map(|col| buffer[(col, row)].symbol())
                .collect::<String>();
            text.find(title)
                .map(|byte| (text[..byte].chars().count() as u16, row))
        })
        .expect("field title is rendered");
    for kind in [
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
    ] {
        dialog.handle_mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        });
    }
    assert!(matches!(dialog.values[0], FieldValue::Boolean(true)));
}

#[test]
fn revise_edits_feedback_inline_and_submits_it_with_the_action() {
    let mut dialog = plan_review(4);

    assert_eq!(dialog.display_fields.len(), 1);
    assert!(!rendered(&dialog).contains("> "));

    dialog.handle_key(KeyCode::Down, KeyModifiers::NONE);
    let revise = rendered(&dialog);
    assert!(revise.contains("● Revise"));
    assert!(revise.contains("Describe what the agent should change"));
    assert!(revise.contains("> "));
    assert!(revise.contains("1/1"));

    dialog.paste("add a regression test");
    dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(
        dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
        Some(ElicitationResponse::Accept {
            content: BTreeMap::from([
                (
                    "feedback".into(),
                    ElicitationValue::String("add a regression test".into())
                ),
                (
                    "question_0".into(),
                    ElicitationValue::String("revise".into())
                ),
            ])
        })
    );
}

#[test]
fn leaving_revise_keeps_its_draft_out_of_the_answer() {
    let mut dialog = plan_review(4);
    dialog.handle_key(KeyCode::Down, KeyModifiers::NONE);
    dialog.paste("stale revision");
    dialog.handle_key(KeyCode::Up, KeyModifiers::NONE);
    dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE);

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

#[test]
fn selecting_an_option_returns_its_wire_value() {
    let mut dialog = ElicitationDialog::new(request(
        ElicitationFieldKind::SingleSelect {
            options: vec![
                ElicitationOption {
                    value: "thin".into(),
                    title: "Thin callers".into(),
                    description: None,
                    preview: None,
                },
                ElicitationOption {
                    value: "dynamic".into(),
                    title: "Dynamic matrix".into(),
                    description: None,
                    preview: None,
                },
            ],
            default: None,
        },
        true,
    ));
    dialog.handle_key(KeyCode::Down, KeyModifiers::NONE);
    dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE);

    assert_eq!(
        dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
        Some(ElicitationResponse::Accept {
            content: BTreeMap::from([(
                "question_0".into(),
                ElicitationValue::String("dynamic".into())
            )])
        })
    );
}

#[test]
fn first_single_select_option_is_the_visible_and_submitted_default() {
    let mut dialog = ElicitationDialog::new(request(
        ElicitationFieldKind::SingleSelect {
            options: vec![
                ElicitationOption {
                    value: "thin".into(),
                    title: "Thin callers".into(),
                    description: None,
                    preview: None,
                },
                ElicitationOption {
                    value: "dynamic".into(),
                    title: "Dynamic matrix".into(),
                    description: None,
                    preview: None,
                },
            ],
            default: None,
        },
        false,
    ));

    let initial = rendered(&dialog);
    assert!(initial.contains("● Thin callers"));
    assert!(initial.contains("○ Dynamic matrix"));

    dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(
        dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
        Some(ElicitationResponse::Accept {
            content: BTreeMap::from([(
                "question_0".into(),
                ElicitationValue::String("thin".into())
            )])
        })
    );
}

#[test]
fn paired_custom_answers_share_their_question_page() {
    let mut dialog = ElicitationDialog::new(paired_request(3, false));

    assert_eq!(dialog.display_fields.len(), 3);
    let first = rendered(&dialog);
    assert!(first.contains("1/3"));
    assert!(first.contains("○ Other"));
    assert!(!first.contains("1/6"));

    dialog.handle_key(KeyCode::Tab, KeyModifiers::NONE);
    let second = rendered(&dialog);
    assert!(second.contains("2/3"));
    assert!(second.contains("Question 2"));
}

#[test]
fn custom_answer_uses_the_adapter_field_instead_of_the_stale_selection() {
    let mut dialog = ElicitationDialog::new(paired_request(1, false));
    dialog.handle_key(KeyCode::Down, KeyModifiers::NONE);
    dialog.handle_key(KeyCode::Down, KeyModifiers::NONE);
    for character in "custom answer".chars() {
        dialog.handle_key(KeyCode::Char(character), KeyModifiers::NONE);
    }
    dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE);

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
    dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE);

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
    dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE);

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
    dialog.handle_key(KeyCode::Tab, KeyModifiers::NONE);
    assert!(rendered(&dialog).contains("2/2"));
    assert!(rendered(&dialog).contains("Other"));
}

#[test]
fn required_text_blocks_submit_until_answered() {
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
    assert_eq!(dialog.focus_index(), 0);
    assert_eq!(dialog.error.as_deref(), Some("Architecture is required"));
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
    }
}

#[test]
fn compact_question_pane_shows_the_focused_field_title_with_its_control() {
    let mut dialog = ElicitationDialog::new(component_form());
    dialog.handle_key(KeyCode::Tab, KeyModifiers::NONE);
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
        assert!(text.contains("Submit"), "actions missing:\n{text}");
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
        text.contains("1/1  Architecture"),
        "field title missing:\n{text}"
    );
    assert!(
        text.contains("Architecture is required"),
        "error missing:\n{text}"
    );
    assert!(text.contains("Submit"), "actions missing:\n{text}");
}

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
fn escape_cancels_the_elicitation() {
    let mut dialog = ElicitationDialog::new(request(
        ElicitationFieldKind::Boolean { default: None },
        false,
    ));
    assert_eq!(
        dialog.handle_key(KeyCode::Esc, KeyModifiers::NONE),
        Some(ElicitationResponse::Cancel)
    );
}

#[test]
fn dismiss_glyph_returns_the_same_cancel_response_as_escape() {
    let mut clicked = ElicitationDialog::new(request(
        ElicitationFieldKind::Boolean { default: None },
        false,
    ));
    let mut terminal = Terminal::new(TestBackend::new(60, 18)).expect("terminal");
    terminal
        .draw(|frame| {
            let area = frame.area();
            render_elicitation_in(frame, &clicked, &mut FrameSurfaces::new(), area, true);
        })
        .expect("draw elicitation");
    let buffer = terminal.backend().buffer();
    let (column, row) = (0..buffer.area.right())
        .flat_map(|column| (0..buffer.area.bottom()).map(move |row| (column, row)))
        .find(|&(column, row)| buffer[(column, row)].symbol() == "×")
        .expect("elicitation dismiss glyph");
    let press = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };
    assert_eq!(clicked.handle_mouse(press), None);
    assert_eq!(
        clicked.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            ..press
        }),
        Some(ElicitationResponse::Cancel)
    );

    let mut escaped = ElicitationDialog::new(request(
        ElicitationFieldKind::Boolean { default: None },
        false,
    ));
    assert_eq!(
        escaped.handle_key(KeyCode::Esc, KeyModifiers::NONE),
        Some(ElicitationResponse::Cancel)
    );
}
