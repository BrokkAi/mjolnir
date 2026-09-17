use super::*;
use crate::components::{FieldEdit, Interaction};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::{Terminal, backend::TestBackend};

fn click(form: &mut Form<u8>, x: u16, y: u16) -> Option<Interaction<u8>> {
    let event = |kind| {
        Event::Mouse(MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        })
    };
    form.handle(&event(MouseEventKind::Down(MouseButton::Left)));
    form.handle(&event(MouseEventKind::Up(MouseButton::Left)))
        .action
}

fn row_text(width: u16, align: RowAlign) -> String {
    let mut form = Form::<u8>::new();
    form.declare(1, ControlKind::Button);
    form.declare(2, ControlKind::Button);
    form.end_frame(1);
    let mut terminal = Terminal::new(TestBackend::new(width, 1)).unwrap();
    terminal
        .draw(|frame| {
            form.begin_frame();
            ButtonRow::render_aligned(
                frame,
                frame.area(),
                &[(1, "First", true), (2, "Last", true)],
                &mut form,
                align,
            );
            form.end_frame(1);
        })
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

#[test]
fn a_right_aligned_row_that_fits_ends_flush_with_the_area() {
    // "  First  " and "  Last  " are 9 and 8 cells, plus one separator
    // between them, so an 18-cell row leaves 12 dead cells on the left.
    let text = row_text(30, RowAlign::Right);
    assert!(text.ends_with("  First     Last  "), "{text:?}");
    assert_eq!(&text[..12], " ".repeat(12), "{text:?}");
}

#[test]
fn a_row_wider_than_its_area_ignores_right_alignment() {
    assert_eq!(row_text(10, RowAlign::Right), row_text(10, RowAlign::Left));
}

fn column_lines(width: u16, height: u16, focused: u8) -> Vec<String> {
    let mut form = Form::<u8>::new();
    form.declare(1, ControlKind::Button);
    form.declare(2, ControlKind::Button);
    form.declare(3, ControlKind::Button);
    form.end_frame(focused);
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| {
            form.begin_frame();
            ButtonColumn::render(
                frame,
                frame.area(),
                &[(1, "First", true), (2, "Widest", true), (3, "Last", true)],
                &mut form,
            );
            form.end_frame(focused);
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    (buffer.area.y..buffer.area.bottom())
        .map(|y| {
            (buffer.area.x..buffer.area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect()
        })
        .collect()
}

#[test]
fn a_stacked_column_gives_every_button_one_row_of_the_widest_label() {
    // "  Widest  " is the widest button at 10 cells, so the 16-cell area
    // leaves six cells on the left of every row.
    assert_eq!(
        column_lines(16, 3, 1),
        [
            format!("{}  First   ", " ".repeat(6)),
            format!("{}  Widest  ", " ".repeat(6)),
            format!("{}  Last    ", " ".repeat(6)),
        ]
    );
}

#[test]
fn a_column_too_short_for_its_buttons_scrolls_to_the_focused_one() {
    assert_eq!(
        column_lines(16, 1, 3),
        [format!("{}  Last    ", " ".repeat(6))]
    );
}

#[test]
fn tabbing_to_a_clipped_button_scrolls_it_into_view() {
    let mut form = Form::new();
    form.declare(1, ControlKind::Button);
    form.declare(2, ControlKind::Button);
    form.end_frame(1);
    let mut terminal = Terminal::new(TestBackend::new(10, 1)).unwrap();
    for selected in [1, 2] {
        terminal
            .draw(|frame| {
                form.begin_frame();
                ButtonRow::render(
                    frame,
                    frame.area(),
                    &[(1, "First", true), (2, "Last", true)],
                    &mut form,
                );
                form.end_frame(1);
            })
            .unwrap();
        assert_eq!(form.focused(), Some(selected));
        assert_eq!(
            click(&mut form, 5, 0),
            Some(Interaction::Activate(selected))
        );
        form.handle(&Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
    }
    let text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.contains("Last"));
}

#[test]
fn wrapped_choice_rows_keep_navigation_and_inline_editor_hitboxes_distinct() {
    let mut form = Form::new();
    form.declare(
        1,
        ControlKind::ChoiceList {
            len: 2,
            selected: 0,
        },
    );
    form.end_frame(1);
    let mut terminal = Terminal::new(TestBackend::new(12, 7)).unwrap();
    let input = TextInput::from_value("a界z");
    terminal
        .draw(|frame| {
            form.begin_frame();
            ChoiceList::render_wrapped(
                frame,
                Rect::new(0, 0, 12, 7),
                &[
                    Line::from("Heading"),
                    Line::from("First option has long text"),
                    Line::from("Second"),
                    Line::from(""),
                ],
                &[None, Some(0), Some(1), None],
                0,
                0,
                &mut form,
                1,
            );
            TextField::render_inline(
                frame,
                Rect::new(2, 5, 8, 1),
                &input,
                false,
                true,
                &mut form,
                1,
            );
            form.end_frame(1);
        })
        .unwrap();
    assert_eq!(
        form.handle(&Event::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE
        )))
        .action,
        Some(Interaction::Select(1, 1))
    );
    assert_eq!(click(&mut form, 1, 4), Some(Interaction::Select(1, 1)));
    assert_eq!(click(&mut form, 1, 0), None);
    let edit = form
        .handle(&Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 5,
            modifiers: KeyModifiers::NONE,
        }))
        .action;
    assert_eq!(edit, Some(Interaction::Edit(1, FieldEdit::Cursor(4))));
}

#[test]
fn drawn_unicode_field_click_uses_display_cells_and_grapheme_boundaries() {
    let mut form = Form::new();
    let mut input = TextInput::from_value("a界e\u{301}z");
    input.set_cursor(0);
    form.declare(1, ControlKind::TextField);
    form.end_frame(1);
    let mut terminal = Terminal::new(TestBackend::new(10, 2)).unwrap();
    terminal
        .draw(|frame| {
            form.begin_frame();
            TextField::render(frame, Rect::new(1, 0, 8, 1), &input, &mut form, 1);
            form.end_frame(1);
        })
        .unwrap();
    assert_eq!(terminal.backend().buffer()[(2, 0)].symbol(), "界");
    let result = form.handle(&Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 4,
        row: 0,
        modifiers: KeyModifiers::NONE,
    }));
    assert_eq!(
        result.action,
        Some(Interaction::Edit(1, FieldEdit::Cursor(4)))
    );
    TextField::apply(&mut input, FieldEdit::Cursor(4));
    assert_eq!(input.cursor(), "a界".len());
}

#[test]
fn multiline_field_click_tracks_wrapped_newlines_and_unicode() {
    let mut form = Form::new();
    let mut input = TextInput::multiline();
    input.set_value("abcdefghi\nab界e\u{301}z\nfinal");
    input.set_cursor(input.value().len());
    form.declare(1, ControlKind::TextField);
    form.end_frame(1);
    let area = Rect::new(2, 1, 8, 2);
    let mut terminal = Terminal::new(TestBackend::new(14, 5)).unwrap();
    terminal
        .draw(|frame| {
            form.begin_frame();
            TextField::render_multiline(frame, area, &input, true, true, &mut form, 1);
            form.end_frame(1);
        })
        .unwrap();

    // The caret is on the final line, so the field scrolls to show the
    // second logical line and the final hard-newline-delimited line.
    let click = |form: &mut Form<i32>, column, row| {
        form.handle(&Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }))
        .action
    };
    let first = click(&mut form, area.x + 4, area.y);
    assert_eq!(
        first,
        Some(Interaction::Edit(
            1,
            FieldEdit::Cursor("abcdefghi\n".len() + "ab界".len())
        ))
    );
    let Some(Interaction::Edit(1, first_edit)) = first else {
        panic!("wrapped row click should edit the field");
    };
    TextField::apply(&mut input, first_edit);
    TextField::apply(
        &mut input,
        FieldEdit::Key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE)),
    );

    input.set_cursor(input.value().len());
    terminal
        .draw(|frame| {
            form.begin_frame();
            TextField::render_multiline(frame, area, &input, true, true, &mut form, 1);
            form.end_frame(1);
        })
        .unwrap();

    let second = click(&mut form, area.x, area.y + 1);
    assert_eq!(
        second,
        Some(Interaction::Edit(
            1,
            FieldEdit::Cursor("abcdefghi\nab界Xe\u{301}z\n".len()),
        ))
    );
    let Some(Interaction::Edit(1, second_edit)) = second else {
        panic!("hard-newline row click should edit the field");
    };
    TextField::apply(&mut input, second_edit);
    TextField::apply(
        &mut input,
        FieldEdit::Key(KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::NONE)),
    );
    assert_eq!(input.value(), "abcdefghi\nab界Xe\u{301}z\nYfinal");
}

#[test]
fn multiline_field_click_scales_with_a_large_pasted_prompt() {
    let mut form = Form::new();
    let mut input = TextInput::multiline();
    input.set_value("x".repeat(70_000));
    input.set_cursor(input.value().len());
    form.declare(1, ControlKind::TextField);
    form.end_frame(1);
    let area = Rect::new(1, 1, 20, 3);
    let mut terminal = Terminal::new(TestBackend::new(24, 6)).unwrap();
    terminal
        .draw(|frame| {
            form.begin_frame();
            TextField::render_multiline(frame, area, &input, true, true, &mut form, 1);
            form.end_frame(1);
        })
        .unwrap();

    let result = form.handle(&Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: area.right() - 1,
        row: area.bottom() - 1,
        modifiers: KeyModifiers::NONE,
    }));
    assert_eq!(
        result.action,
        Some(Interaction::Edit(1, FieldEdit::Cursor(70_000)))
    );
    let Some(Interaction::Edit(1, edit)) = result.action else {
        panic!("large prompt click should edit the field");
    };
    TextField::apply(&mut input, edit);
    TextField::apply(
        &mut input,
        FieldEdit::Key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE)),
    );
    assert_eq!(input.value().len(), 70_001);
    assert_eq!(&input.value()[69_995..], "xxxxx!");
}

#[test]
fn multiline_field_click_after_a_zero_width_tab_keeps_the_byte_offset() {
    let mut form = Form::new();
    let mut input = TextInput::multiline();
    input.set_value("a\t界");
    input.set_cursor(input.value().len());
    form.declare(1, ControlKind::TextField);
    form.end_frame(1);
    let area = Rect::new(1, 0, 8, 1);
    let mut terminal = Terminal::new(TestBackend::new(10, 2)).unwrap();
    terminal
        .draw(|frame| {
            form.begin_frame();
            TextField::render_multiline(frame, area, &input, true, true, &mut form, 1);
            form.end_frame(1);
        })
        .unwrap();

    // Ratatui skips the tab control character, so the visible wide glyph
    // starts in the same cell as the tab's end boundary.
    let result = form.handle(&Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: area.x + 1,
        row: area.y,
        modifiers: KeyModifiers::NONE,
    }));
    assert_eq!(
        result.action,
        Some(Interaction::Edit(1, FieldEdit::Cursor("a\t".len())))
    );
    let Some(Interaction::Edit(1, edit)) = result.action else {
        panic!("tab click should edit the field");
    };
    TextField::apply(&mut input, edit);
    TextField::apply(
        &mut input,
        FieldEdit::Key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE)),
    );
    assert_eq!(input.value(), "a\tX界");
}

#[test]
fn scrolling_tabs_hit_the_drawn_label_and_ignore_separator() {
    let mut form = Form::new();
    form.declare(
        1,
        ControlKind::Tabs {
            len: 3,
            selected: 2,
        },
    );
    form.end_frame(1);
    let mut terminal = Terminal::new(TestBackend::new(10, 1)).unwrap();
    terminal
        .draw(|frame| {
            form.begin_frame();
            TabStrip::render(
                frame,
                Rect::new(0, 0, 10, 1),
                &["Long first", "界", "Last"],
                2,
                &mut form,
                1,
            );
            form.end_frame(1);
        })
        .unwrap();
    assert_eq!(terminal.backend().buffer()[(3, 0)].symbol(), "界");
    assert_eq!(terminal.backend().buffer()[(6, 0)].symbol(), "L");
    assert_eq!(click(&mut form, 5, 0), None);
    assert_eq!(click(&mut form, 3, 0), Some(Interaction::Select(1, 1)));
    assert_eq!(click(&mut form, 6, 0), Some(Interaction::Select(1, 2)));
}

#[test]
fn clipped_list_click_selects_the_visible_row_after_scrolling() {
    let mut form = Form::new();
    form.declare(
        1,
        ControlKind::ChoiceList {
            len: 20,
            selected: 19,
        },
    );
    form.end_frame(1);
    let lines = (0..20)
        .map(|index| Line::raw(format!("row {index}")))
        .collect::<Vec<_>>();
    let mut terminal = Terminal::new(TestBackend::new(15, 3)).unwrap();
    terminal
        .draw(|frame| {
            form.begin_frame();
            ChoiceList::render(frame, frame.area(), &lines, 19, &mut form, 1);
            form.end_frame(1);
        })
        .unwrap();
    assert_eq!(form.list_offset(1), 17);
    assert_eq!(click(&mut form, 1, 0), Some(Interaction::Select(1, 17)));
}

#[test]
fn combobox_state_consumes_preview_and_returns_commit_or_dismissal() {
    let mut state = ComboBoxState::default();
    state.open(1, 0);
    assert_eq!(state.selection(1, 2), 0);
    assert_eq!(state.route(Some(Interaction::Select(1, 2))), None);
    assert_eq!(state.selection(1, 0), 2);
    assert_eq!(
        state.route(Some(Interaction::ComboBoxCommit(1, 1))),
        Some(Interaction::ComboBoxCommit(1, 1))
    );
    assert_eq!(state.open_id(), None);

    state.open(1, 1);
    assert_eq!(
        state.route(Some(Interaction::ComboBoxDismiss(1))),
        Some(Interaction::ComboBoxDismiss(1))
    );
    assert!(!state.is_open(1));
    assert_eq!(
        state.route(Some(Interaction::Activate(2))),
        Some(Interaction::Activate(2))
    );
}

#[test]
fn combobox_collapsed_value_keeps_a_visible_dropdown_glyph() {
    assert_eq!(clipped_display("long value", 1), ComboBox::GLYPH);
    let mut terminal = Terminal::new(TestBackend::new(12, 1)).expect("terminal");
    terminal
        .draw(|frame| {
            let mut form = Form::new();
            form.begin_frame();
            ComboBox::render(
                frame,
                frame.area(),
                Rect::new(0, 0, 6, 1),
                "long value",
                &[Line::raw("one"), Line::raw("two")],
                0,
                false,
                true,
                " choices ",
                PopupSide::Below,
                &mut form,
                1,
            );
            form.end_frame(1);
        })
        .expect("draw combobox");
    let rendered = (0..6)
        .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
        .collect::<String>();
    assert!(rendered.contains(ComboBox::GLYPH), "{rendered}");
}

#[test]
fn rendered_combobox_popup_rows_commit_with_a_mouse_click() {
    let mut terminal = Terminal::new(TestBackend::new(20, 10)).expect("terminal");
    let mut form = Form::new();
    terminal
        .draw(|frame| {
            form.begin_frame();
            ComboBox::render(
                frame,
                frame.area(),
                Rect::new(0, 1, 10, 1),
                "one",
                &[Line::raw("one"), Line::raw("two")],
                0,
                true,
                true,
                " choices ",
                PopupSide::Below,
                &mut form,
                1,
            );
            form.end_frame(1);
        })
        .expect("draw popup");
    assert_eq!(
        click(&mut form, 1, 4),
        Some(Interaction::ComboBoxCommit(1, 1))
    );
}
