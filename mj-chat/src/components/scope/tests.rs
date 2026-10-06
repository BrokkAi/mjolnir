use super::*;
use crate::components::test_support::key;

fn form() -> Form<u8> {
    let mut form = Form::new();
    form.register(1, ControlKind::TextField, Rect::new(0, 0, 5, 1), true);
    form.register(2, ControlKind::Button, Rect::new(0, 1, 5, 1), true);
    form.end_frame(1);
    form
}

fn list_form() -> Form<u8> {
    let mut form = Form::new();
    form.register(
        1,
        ControlKind::ChoiceList {
            len: 3,
            selected: 0,
        },
        Rect::new(0, 0, 10, 3),
        true,
    );
    form.set_list_contents(1, vec!["Alpha".into(), "Beta".into(), "Gamma".into()]);
    form.end_frame(1);
    form
}

fn click_at(form: &mut Form<u8>, row: u16, at: Instant) -> Option<Interaction<u8>> {
    form.handle_at(&mouse(MouseEventKind::Down(MouseButton::Left), 1, row), at);
    form.handle_at(&mouse(MouseEventKind::Up(MouseButton::Left), 1, row), at)
        .action
}

#[test]
fn different_rows_expired_clicks_and_navigation_do_not_activate() {
    let mut form = list_form();
    let now = Instant::now();
    click_at(&mut form, 0, now);
    assert_eq!(click_at(&mut form, 1, now), Some(Interaction::Select(1, 1)));
    assert_eq!(
        click_at(&mut form, 1, now + Duration::from_millis(501)),
        Some(Interaction::Select(1, 1))
    );
    form.handle_at(&key(KeyCode::Down), now + Duration::from_millis(502));
    assert_eq!(
        click_at(&mut form, 1, now + Duration::from_millis(503)),
        Some(Interaction::Select(1, 1))
    );
}

#[test]
fn identical_labels_with_new_identities_do_not_activate_the_replacement() {
    let mut form = list_form();
    let now = Instant::now();
    form.set_list_identity(1, "old ids".into());
    click_at(&mut form, 1, now);
    form.set_list_identity(1, "replacement ids".into());
    assert_eq!(click_at(&mut form, 1, now), Some(Interaction::Select(1, 1)));
}

#[test]
fn a_disabled_row_between_clicks_breaks_the_pair() {
    let mut form = list_form();
    form.controls[0].row_enabled = vec![true, false, true];
    let now = Instant::now();
    click_at(&mut form, 0, now);
    assert_eq!(click_at(&mut form, 1, now), None);
    assert_eq!(click_at(&mut form, 0, now), Some(Interaction::Select(1, 0)));
}

#[test]
fn changed_contents_and_resize_invalidate_double_clicks() {
    let mut form = list_form();
    let now = Instant::now();
    click_at(&mut form, 1, now);
    form.set_list_contents(
        1,
        vec!["Alpha".into(), "Different item".into(), "Gamma".into()],
    );
    assert_eq!(click_at(&mut form, 1, now), Some(Interaction::Select(1, 1)));
    form.handle_at(&Event::Resize(80, 24), now);
    assert_eq!(click_at(&mut form, 1, now), Some(Interaction::Select(1, 1)));
}

#[test]
fn moving_between_rows_during_a_press_does_not_select_or_activate() {
    let mut form = list_form();
    form.handle(&mouse(MouseEventKind::Down(MouseButton::Left), 1, 0));
    form.handle(&mouse(MouseEventKind::Drag(MouseButton::Left), 1, 1));
    assert_eq!(
        form.handle(&mouse(MouseEventKind::Up(MouseButton::Left), 1, 1))
            .action,
        None
    );
    assert_eq!(form.selected(1), Some(0));
}

#[test]
fn ordinary_redraw_preserves_double_click_but_a_clone_does_not() {
    let mut form = list_form();
    let now = Instant::now();
    click_at(&mut form, 1, now);
    let mut clone = form.clone();
    assert_eq!(
        click_at(&mut clone, 1, now),
        Some(Interaction::Select(1, 1))
    );
    form.reset_geometry();
    form.begin_frame();
    form.register(
        1,
        ControlKind::ChoiceList {
            len: 3,
            selected: 1,
        },
        Rect::new(0, 0, 10, 3),
        true,
    );
    form.end_frame(1);
    assert_eq!(
        click_at(&mut form, 1, now + DOUBLE_CLICK_INTERVAL),
        Some(Interaction::Activate(1))
    );
}

#[test]
fn field_space_is_editing_and_unicode_cursor_is_applied_by_editor() {
    let mut form = form();
    let space = key(KeyCode::Char(' '));
    assert_eq!(
        form.handle(&space).action,
        Some(Interaction::Edit(
            1,
            FieldEdit::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE,))
        ))
    );
    let mut input = TextInput::from_value("a👩‍💻b");
    assert_eq!(
        apply_field_edit(&mut input, FieldEdit::Cursor(2)),
        EditOutcome::Changed
    );
    assert_eq!(input.cursor(), 1);
}

fn mouse(kind: MouseEventKind, x: u16, y: u16) -> Event {
    Event::Mouse(crossterm::event::MouseEvent {
        kind,
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    })
}

#[test]
fn focus_repairs_after_removal_and_disabling_every_control() {
    let mut form = form();
    form.focus(2);
    form.begin_frame();
    form.register(1, ControlKind::Button, Rect::default(), true);
    form.register(3, ControlKind::Button, Rect::default(), true);
    form.end_frame(1);
    assert_eq!(form.focused(), Some(3));
    form.begin_frame();
    form.register(1, ControlKind::Button, Rect::default(), false);
    form.register(3, ControlKind::Button, Rect::default(), false);
    form.end_frame(1);
    assert_eq!(form.focused(), None);
    assert_eq!(form.handle(&key(KeyCode::Enter)).action, None);
    form.begin_frame();
    form.register(1, ControlKind::Button, Rect::default(), true);
    form.end_frame(1);
    assert_eq!(form.focused(), Some(1));
}

#[test]
fn pointer_release_outside_or_after_disappearance_never_activates() {
    let mut form = form();
    let down = mouse(MouseEventKind::Down(MouseButton::Left), 1, 1);
    let up = mouse(MouseEventKind::Up(MouseButton::Left), 1, 1);
    assert!(form.handle(&down).consumed);
    assert_eq!(form.focused(), Some(2));
    assert!(form.captures_pointer());
    assert_eq!(
        form.handle(&up).action,
        Some(Interaction::Activate(2)),
        "a matching release activates the captured button once"
    );
    assert!(!form.captures_pointer());
    assert!(form.handle(&up).action.is_none());

    form.handle(&down);
    let outside = form.handle(&mouse(MouseEventKind::Up(MouseButton::Left), 20, 20));
    assert!(outside.consumed);
    assert_eq!(outside.action, None);
    assert!(!form.captures_pointer());
    assert!(form.handle(&up).action.is_none());
    form.handle(&down);
    form.begin_frame();
    form.register(1, ControlKind::TextField, Rect::default(), true);
    form.end_frame(1);
    assert!(!form.captures_pointer());
    assert!(form.handle(&up).action.is_none());
}

#[test]
fn metadata_updates_preserve_cursor_maps_and_pressed_controls() {
    let mut form = form();
    form.register_with_cursor_map(
        1,
        ControlKind::TextField,
        Rect::new(0, 0, 5, 1),
        true,
        vec![(0, 0), (3, 4)],
    );
    form.handle(&mouse(MouseEventKind::Down(MouseButton::Left), 1, 1));
    form.begin_update();
    form.declare(1, ControlKind::TextField);
    form.declare(2, ControlKind::Button);
    form.end_frame(1);
    assert_eq!(
        form.handle(&mouse(MouseEventKind::Up(MouseButton::Left), 1, 1))
            .action,
        Some(Interaction::Activate(2))
    );
    assert_eq!(
        form.handle(&mouse(MouseEventKind::Down(MouseButton::Left), 3, 0))
            .action,
        Some(Interaction::Edit(1, FieldEdit::Cursor(4)))
    );
}

#[test]
fn capture_survives_metadata_reconciliation_before_redraw() {
    let mut form = form();
    form.handle(&mouse(MouseEventKind::Down(MouseButton::Left), 1, 1));
    form.reset_geometry();
    form.begin_update();
    form.declare(1, ControlKind::TextField);
    form.declare(2, ControlKind::Button);
    form.end_frame(1);
    assert!(form.captures_pointer());
    form.begin_frame();
    form.register(1, ControlKind::TextField, Rect::new(0, 0, 5, 1), true);
    form.register(2, ControlKind::Button, Rect::new(0, 1, 5, 1), true);
    form.end_frame(1);
    assert_eq!(
        form.handle(&mouse(MouseEventKind::Up(MouseButton::Left), 1, 1))
            .action,
        Some(Interaction::Activate(2))
    );
}

#[test]
fn changed_list_metadata_drops_obsolete_row_mappings() {
    let mut form = Form::new();
    form.register_with_rows(
        1,
        ControlKind::ChoiceList {
            len: 2,
            selected: 0,
        },
        Rect::new(0, 0, 4, 2),
        true,
        vec![Some(0), Some(1)],
        vec![true, true],
    );
    form.end_frame(1);
    form.begin_update();
    form.declare(
        1,
        ControlKind::ChoiceList {
            len: 4,
            selected: 0,
        },
    );
    form.end_frame(1);
    assert_eq!(
        form.handle(&key(KeyCode::End)).action,
        Some(Interaction::Select(1, 3))
    );
    assert!(!form.contains(1, 1));
}

#[test]
fn activation_ignores_repeat_release_and_modified_space() {
    let mut form = form();
    form.focus(2);
    for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
        assert!(
            form.handle(&Event::Key(KeyEvent::new_with_kind(
                KeyCode::Enter,
                KeyModifiers::NONE,
                kind
            )))
            .action
            .is_none()
        );
    }
    assert!(
        form.handle(&Event::Key(KeyEvent::new(
            KeyCode::Char(' '),
            KeyModifiers::CONTROL
        )))
        .action
        .is_none()
    );
    assert_eq!(
        form.handle(&key(KeyCode::Char(' '))).action,
        Some(Interaction::Activate(2))
    );
}

#[test]
fn dismiss_drag_or_move_outside_disarms_without_cancel() {
    let mut form = Form::<u8>::new();
    form.register_dismiss(Rect::new(1, 0, 3, 1), true);
    form.end_frame(1);
    form.handle(&mouse(MouseEventKind::Down(MouseButton::Left), 2, 0));
    assert!(form.dismiss_is_armed());
    assert_eq!(
        form.handle(&mouse(MouseEventKind::Moved, 20, 20)).action,
        None
    );
    assert!(!form.dismiss_is_armed());
    assert!(!form.captures_pointer());
    assert_eq!(
        form.handle(&mouse(MouseEventKind::Up(MouseButton::Left), 2, 0))
            .action,
        None
    );
}

#[test]
fn dismiss_geometry_does_not_leave_a_stale_capture_across_frames() {
    let mut form = Form::<u8>::new();
    form.register_dismiss(Rect::new(1, 0, 3, 1), true);
    form.end_frame(1);
    form.handle(&mouse(MouseEventKind::Down(MouseButton::Left), 2, 0));
    form.reset_geometry();
    assert!(!form.contains(2, 0));
    assert_eq!(
        form.handle(&mouse(MouseEventKind::Up(MouseButton::Left), 2, 0))
            .action,
        None
    );
    form.begin_frame();
    form.end_frame(1);
    assert!(!form.contains(2, 0));
    assert!(!form.captures_pointer());
}

/// A path field with a following button, so focus has somewhere to go.
fn path_form(len: usize, expanded: bool) -> Form<u8> {
    let mut form = Form::new();
    form.register(
        1,
        ControlKind::PathField {
            len,
            selected: 0,
            expanded,
        },
        Rect::new(0, 0, 10, 1),
        true,
    );
    form.register(2, ControlKind::Button, Rect::new(0, 8, 6, 1), true);
    form.end_frame(1);
    form
}

fn path_field_golden_buffer(
    input: &crate::path_input::PathInput,
    form: &mut Form<u8>,
    width: u16,
    height: u16,
    focused: u8,
) -> ratatui::buffer::Buffer {
    use ratatui::{Terminal, backend::TestBackend};

    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal
        .draw(|frame| {
            form.begin_frame();
            crate::components::PathField::render(frame, Rect::new(0, 1, 12, 1), input, form, 1);
            crate::components::Button::render(
                frame,
                Rect::new(0, 8, 10, 1),
                "Continue",
                true,
                form,
                2,
            );
            form.end_frame(focused);
        })
        .expect("draw path field");
    terminal.backend().buffer().clone()
}

fn append_path_golden_state(
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
fn golden_path_field_completion() {
    use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEventKind};
    use mj_core::path_completion::PathCompletion;

    let mut output = String::new();
    let mut input = crate::path_input::PathInput::from_value("~/p".to_owned());
    let mut form = path_form(0, false);
    let buffer = path_field_golden_buffer(&input, &mut form, 40, 14, 1);
    append_path_golden_state(&mut output, "collapsed path field", 40, 14, &buffer, &[]);

    let completion = form.handle(&chord(KeyCode::Char(' '), KeyModifiers::CONTROL));
    let requested = input.request_completion();
    let buffer = path_field_golden_buffer(&input, &mut form, 40, 14, 1);
    append_path_golden_state(
        &mut output,
        "ctrl space requests completion",
        40,
        14,
        &buffer,
        &[
            format!("action: {:?}", completion.action),
            format!("request prefix: {requested:?}"),
        ],
    );
    let nul = form.handle(&key(KeyCode::Null));
    let duplicate_request = input.request_completion();
    append_path_golden_state(
        &mut output,
        "terminal NUL completion key",
        40,
        14,
        &path_field_golden_buffer(&input, &mut form, 40, 14, 1),
        &[
            format!("action: {:?}", nul.action),
            format!("duplicate request: {duplicate_request:?}"),
        ],
    );

    input.apply_completion(
        "~/p",
        PathCompletion {
            candidates: vec!["~/projects/".into(), "~/provision/".into()],
            insert: None,
            truncated: false,
        },
    );
    append_path_golden_state(
        &mut output,
        "completion candidates",
        40,
        14,
        &path_field_golden_buffer(&input, &mut form, 40, 14, 1),
        &[],
    );
    let selected = form.handle(&key(KeyCode::Down));
    if let Some(crate::components::Interaction::Select(_, index)) = selected.action.as_ref() {
        input.select_completion(*index);
    }
    append_path_golden_state(
        &mut output,
        "keyboard previews second candidate",
        40,
        14,
        &path_field_golden_buffer(&input, &mut form, 40, 14, 1),
        &[format!("action: {:?}", selected.action)],
    );
    let accepted = form.handle(&key(KeyCode::Enter));
    let committed = input.accept_completion();
    append_path_golden_state(
        &mut output,
        "enter commits candidate",
        40,
        14,
        &path_field_golden_buffer(&input, &mut form, 40, 14, 1),
        &[
            format!("action: {:?}", accepted.action),
            format!("accepted: {committed}"),
        ],
    );

    let mut edited = crate::path_input::PathInput::from_value(String::new());
    let mut edit_form = path_form(0, false);
    let edit = edit_form.handle(&key(KeyCode::Char('a')));
    if let Some(crate::components::Interaction::Edit(_, edit)) = edit.action.clone() {
        crate::components::PathField::apply(&mut edited, edit);
    }
    append_path_golden_state(
        &mut output,
        "collapsed path edits as text",
        40,
        14,
        &path_field_golden_buffer(&edited, &mut edit_form, 40, 14, 1),
        &[format!("action: {:?}", edit.action)],
    );
    let submit = edit_form.handle(&key(KeyCode::Enter));
    append_path_golden_state(
        &mut output,
        "collapsed path enter activates",
        40,
        14,
        &path_field_golden_buffer(&edited, &mut edit_form, 40, 14, 1),
        &[format!("action: {:?}", submit.action)],
    );

    let mut escaped = crate::path_input::PathInput::from_value("~/p".to_owned());
    escaped.request_completion();
    escaped.apply_completion(
        "~/p",
        PathCompletion {
            candidates: vec!["~/projects/".into(), "~/provision/".into()],
            insert: None,
            truncated: false,
        },
    );
    let mut escape_form = path_form(2, true);
    let dismissal = escape_form.handle(&key(KeyCode::Esc));
    escaped.dismiss_completion();
    append_path_golden_state(
        &mut output,
        "escape dismisses candidates",
        40,
        14,
        &path_field_golden_buffer(&escaped, &mut escape_form, 40, 14, 1),
        &[format!("action: {:?}", dismissal.action)],
    );

    let mut tabbed = crate::path_input::PathInput::from_value("~/p".to_owned());
    tabbed.request_completion();
    tabbed.apply_completion(
        "~/p",
        PathCompletion {
            candidates: vec!["~/projects/".into(), "~/provision/".into()],
            insert: None,
            truncated: false,
        },
    );
    let mut tab_form = path_form(2, true);
    let dismissal = tab_form.handle(&key(KeyCode::Tab));
    tabbed.dismiss_completion();
    append_path_golden_state(
        &mut output,
        "tab dismisses and focuses next control",
        40,
        14,
        &path_field_golden_buffer(&tabbed, &mut tab_form, 40, 14, 2),
        &[
            format!("action: {:?}", dismissal.action),
            format!("focused control: {:?}", tab_form.focused()),
        ],
    );

    let mut clicked = crate::path_input::PathInput::from_value("~/p".to_owned());
    clicked.request_completion();
    clicked.apply_completion(
        "~/p",
        PathCompletion {
            candidates: vec!["~/projects/".into(), "~/provision/".into()],
            insert: None,
            truncated: false,
        },
    );
    let mut click_form = path_form(2, true);
    let _ = path_field_golden_buffer(&clicked, &mut click_form, 40, 14, 1);
    let popup = click_form
        .controls
        .iter()
        .find(|control| control.id == 1)
        .expect("path field registration")
        .popup_area;
    let x = popup.x.saturating_add(1);
    let y = popup.y.saturating_add(1);
    click_form.handle(&mouse(MouseEventKind::Down(MouseButton::Left), x, y));
    let click = click_form
        .handle(&mouse(MouseEventKind::Up(MouseButton::Left), x, y))
        .action;
    if let Some(crate::components::Interaction::PathCommit(_, index)) = click.as_ref() {
        clicked.select_completion(*index);
        clicked.accept_completion();
    }
    append_path_golden_state(
        &mut output,
        "mouse click commits candidate",
        40,
        14,
        &path_field_golden_buffer(&clicked, &mut click_form, 40, 14, 1),
        &[format!("action: {click:?}")],
    );

    let mut outside = crate::path_input::PathInput::from_value("~/p".to_owned());
    outside.request_completion();
    outside.apply_completion(
        "~/p",
        PathCompletion {
            candidates: vec!["~/projects/".into(), "~/provision/".into()],
            insert: None,
            truncated: false,
        },
    );
    let mut outside_form = path_form(2, true);
    let dismissal = outside_form.handle(&mouse(MouseEventKind::Down(MouseButton::Left), 1, 8));
    outside.dismiss_completion();
    append_path_golden_state(
        &mut output,
        "outside click dismisses candidates",
        40,
        14,
        &path_field_golden_buffer(&outside, &mut outside_form, 40, 14, 2),
        &[format!("action: {:?}", dismissal.action)],
    );

    mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), "path-field-completion", &output);
}

/// A key press carrying modifiers, which the shared `key` fixture cannot make.
fn chord(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, modifiers))
}

// Hard-won: 20b5749a: an open popup let a click activate the control it covered.
#[test]
fn open_popup_takes_clicks_over_a_later_control_it_covers() {
    let mut form = Form::new();
    form.register_combobox(
        1,
        ControlKind::ComboBox {
            len: 3,
            selected: 0,
            expanded: true,
        },
        Rect::new(0, 0, 8, 1),
        true,
        Rect::new(0, 1, 10, 5),
        vec![None, Some(0), Some(1), Some(2), None],
    );
    form.register(2, ControlKind::Button, Rect::new(0, 3, 8, 1), true);
    form.end_frame(1);
    form.handle(&mouse(MouseEventKind::Down(MouseButton::Left), 1, 3));
    assert_eq!(
        form.handle(&mouse(MouseEventKind::Up(MouseButton::Left), 1, 3))
            .action,
        Some(Interaction::ComboBoxCommit(1, 1))
    );
}
