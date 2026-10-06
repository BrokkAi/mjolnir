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

// The fixed renderer needs each visible control and viewport option together.
#[allow(clippy::too_many_arguments)]
fn combobox_golden_buffer(
    form: &mut Form<u8>,
    value: &str,
    options: &[Line<'_>],
    selected: usize,
    expanded: bool,
    width: u16,
    height: u16,
    focus: u8,
    with_button: bool,
) -> ratatui::buffer::Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal
        .draw(|frame| {
            form.begin_frame();
            let field_y = if height <= 1 { 0 } else { 1 };
            ComboBox::render(
                frame,
                frame.area(),
                Rect::new(0, field_y, 12, 1),
                value,
                options,
                selected,
                expanded,
                true,
                " choices ",
                PopupSide::Below,
                form,
                1,
            );
            if with_button {
                Button::render(frame, Rect::new(14, 1, 14, 1), "Continue", true, form, 2);
            }
            form.end_frame(focus);
        })
        .expect("draw combobox");
    terminal.backend().buffer().clone()
}

fn append_component_golden_state(
    output: &mut String,
    label: &str,
    width: u16,
    height: u16,
    buffer: &ratatui::buffer::Buffer,
    details: &[String],
) {
    use std::fmt::Write as _;

    if !output.is_empty() {
        output.push('\n');
    }
    writeln!(output, "=== {label} ({width}x{height}) ===").expect("write state header");
    output.push_str(&crate::golden::buffer_lines(buffer).join("\n"));
    output.push('\n');
    for detail in details {
        writeln!(output, "{detail}").expect("write state detail");
    }
}

#[test]
fn golden_combobox_popup() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent};

    let mut output = String::new();
    let options = [Line::raw("one"), Line::raw("two"), Line::raw("three")];

    let mut form = Form::new();
    let mut state = ComboBoxState::default();
    let mut committed = 0;
    let buffer = combobox_golden_buffer(
        &mut form, "one", &options, committed, false, 20, 10, 1, true,
    );
    let opened = form.handle(&Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    state.open(1, committed);
    append_component_golden_state(
        &mut output,
        "collapsed field opens",
        20,
        10,
        &buffer,
        &[format!("action: {:?}", opened.action)],
    );
    let buffer = combobox_golden_buffer(
        &mut form,
        "one",
        &options,
        state.selection(1, committed),
        state.is_open(1),
        20,
        10,
        1,
        true,
    );
    append_component_golden_state(&mut output, "popup opened", 20, 10, &buffer, &[]);
    let preview = form.handle(&Event::Key(KeyEvent::new(
        KeyCode::Down,
        KeyModifiers::NONE,
    )));
    let routed_preview = state.route(preview.action);
    let selected = state.selection(1, committed);
    let buffer =
        combobox_golden_buffer(&mut form, "one", &options, selected, true, 20, 10, 1, true);
    append_component_golden_state(
        &mut output,
        "keyboard preview leaves committed value",
        20,
        10,
        &buffer,
        &[
            format!("routed preview: {routed_preview:?}"),
            format!("committed selection: {committed}"),
            format!("preview selection: {selected}"),
        ],
    );
    let commit = form.handle(&Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
    let routed_commit = state.route(commit.action);
    if let Some(Interaction::ComboBoxCommit(_, selection)) = routed_commit.as_ref() {
        committed = *selection;
    }
    let buffer = combobox_golden_buffer(
        &mut form, "two", &options, committed, false, 20, 10, 1, true,
    );
    append_component_golden_state(
        &mut output,
        "tab commits highlighted option",
        20,
        10,
        &buffer,
        &[
            format!("routed action: {routed_commit:?}"),
            format!("committed selection: {committed}"),
        ],
    );
    form.focus(2);
    let button = combobox_golden_buffer(
        &mut form, "two", &options, committed, false, 30, 10, 2, true,
    );
    let action = form.handle(&Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    let routed = state.route(action.action);
    append_component_golden_state(
        &mut output,
        "unrelated button action passes through",
        30,
        10,
        &button,
        &[format!("routed action: {routed:?}")],
    );

    let mut tiny_form = Form::new();
    let tiny = combobox_golden_buffer(
        &mut tiny_form,
        "long value",
        &options,
        0,
        false,
        6,
        1,
        1,
        false,
    );
    append_component_golden_state(
        &mut output,
        "collapsed glyph at minimum width",
        6,
        1,
        &tiny,
        &[],
    );

    let mut form = Form::new();
    let opened = form.handle(&Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    let _ = opened;
    let popup = combobox_golden_buffer(&mut form, "one", &options, 0, true, 20, 10, 1, false);
    let selected = click(&mut form, 1, 4);
    append_component_golden_state(
        &mut output,
        "mouse click commits popup row",
        20,
        10,
        &popup,
        &[format!("action: {selected:?}")],
    );

    let mut form = Form::new();
    let buffer = combobox_golden_buffer(&mut form, "model", &options, 0, true, 20, 10, 1, false);
    let event = Event::Mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: 1,
        row: 3,
        modifiers: KeyModifiers::NONE,
    });
    let selected = form.handle(&event);
    append_component_golden_state(
        &mut output,
        "popup scroll selects an option",
        20,
        10,
        &buffer,
        &[format!("action: {:?}", selected.action)],
    );
    let dismissed = form.handle(&Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    let dismissed_buffer =
        combobox_golden_buffer(&mut form, "model", &options, 1, false, 20, 10, 1, false);
    append_component_golden_state(
        &mut output,
        "escape dismisses expanded popup",
        20,
        10,
        &dismissed_buffer,
        &[format!("action: {:?}", dismissed.action)],
    );

    let mut form = Form::new();
    let _ = combobox_golden_buffer(&mut form, "one", &options, 0, true, 20, 10, 1, false);
    let dismissed = click(&mut form, 1, 1);
    let collapsed = combobox_golden_buffer(&mut form, "one", &options, 0, false, 20, 10, 1, false);
    append_component_golden_state(
        &mut output,
        "outside click dismisses expanded popup",
        20,
        10,
        &collapsed,
        &[format!("action: {dismissed:?}")],
    );

    let large_options = (0..30)
        .map(|index| Line::raw(format!("model-{index:02}")))
        .collect::<Vec<_>>();
    for selected in 0..30 {
        let mut form = Form::new();
        let buffer = combobox_golden_buffer(
            &mut form,
            "model",
            &large_options,
            selected,
            true,
            30,
            14,
            1,
            false,
        );
        append_component_golden_state(
            &mut output,
            &format!("selection centered or list pinned ({selected})"),
            30,
            14,
            &buffer,
            &[],
        );
    }
    for selected in 0..6 {
        let options = (0..6)
            .map(|index| Line::raw(format!("model-{index:02}")))
            .collect::<Vec<_>>();
        let mut form = Form::new();
        let buffer = combobox_golden_buffer(
            &mut form, "model", &options, selected, true, 30, 14, 1, false,
        );
        append_component_golden_state(
            &mut output,
            &format!("fitting list selection ({selected})"),
            30,
            14,
            &buffer,
            &[],
        );
    }

    crate::theme::with_symbols(crate::theme::SymbolSet::Ascii, || {
        for selected in [0, 19] {
            let options = (0..20)
                .map(|index| Line::raw(format!("model-{index:02}")))
                .collect::<Vec<_>>();
            let mut form = Form::new();
            let buffer = combobox_golden_buffer(
                &mut form, "model", &options, selected, true, 30, 14, 1, false,
            );
            append_component_golden_state(
                &mut output,
                &format!("ASCII scrollbar thumb ({selected})"),
                30,
                14,
                &buffer,
                &[],
            );
        }
    });

    let options = (0..5)
        .map(|index| Line::raw(format!("model-{index:02}")))
        .collect::<Vec<_>>();
    let mut form = Form::new();
    let buffer = combobox_golden_buffer(&mut form, "model", &options, 2, true, 30, 14, 1, false);
    append_component_golden_state(
        &mut output,
        "no scrollbar without overflow",
        30,
        14,
        &buffer,
        &[],
    );

    crate::theme::with_symbols(crate::theme::SymbolSet::Unicode, || {
        for selected in [0, 10, 19] {
            let options = (0..20)
                .map(|index| Line::raw(format!("model-{index:02}")))
                .collect::<Vec<_>>();
            let mut form = Form::new();
            let buffer = combobox_golden_buffer(
                &mut form, "model", &options, selected, true, 30, 14, 1, false,
            );
            append_component_golden_state(
                &mut output,
                &format!("Unicode scrollbar thumb ({selected})"),
                30,
                14,
                &buffer,
                &[format!("selected option: model-{selected:02}")],
            );
        }
    });

    mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), "combobox-popup", &output);
}
