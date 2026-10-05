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

#[test]
fn combobox_keys_preview_without_commit_until_acceptance() {
    let mut form = Form::new();
    form.register(
        1,
        ControlKind::ComboBox {
            len: 3,
            selected: 0,
            expanded: false,
        },
        Rect::new(0, 0, 8, 1),
        true,
    );
    form.end_frame(1);
    assert_eq!(
        form.handle(&key(KeyCode::Enter)).action,
        Some(Interaction::Activate(1))
    );

    form.begin_frame();
    form.register(
        1,
        ControlKind::ComboBox {
            len: 3,
            selected: 0,
            expanded: true,
        },
        Rect::new(0, 0, 8, 1),
        true,
    );
    form.end_frame(1);
    assert_eq!(
        form.handle(&key(KeyCode::Down)).action,
        Some(Interaction::Select(1, 1))
    );
    assert_eq!(form.selected(1), Some(1));
    assert_eq!(
        form.handle(&key(KeyCode::Tab)).action,
        Some(Interaction::ComboBoxCommit(1, 1))
    );
}

#[test]
fn expanded_combobox_escape_is_local_and_popup_rows_commit() {
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
        Rect::new(0, 2, 10, 5),
        vec![None, Some(0), Some(1), Some(2), None],
    );
    form.end_frame(1);
    assert_eq!(
        form.handle(&mouse(MouseEventKind::ScrollDown, 1, 3)).action,
        Some(Interaction::Select(1, 1))
    );
    assert_eq!(
        form.handle(&key(KeyCode::Esc)).action,
        Some(Interaction::ComboBoxDismiss(1))
    );

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
        Rect::new(0, 2, 10, 5),
        vec![None, Some(0), Some(1), Some(2), None],
    );
    form.end_frame(1);
    form.handle(&mouse(MouseEventKind::Down(MouseButton::Left), 1, 4));
    assert_eq!(
        form.handle(&mouse(MouseEventKind::Up(MouseButton::Left), 1, 4))
            .action,
        Some(Interaction::ComboBoxCommit(1, 1))
    );
    form.handle(&mouse(MouseEventKind::Down(MouseButton::Left), 1, 0));
    assert_eq!(
        form.handle(&mouse(MouseEventKind::Up(MouseButton::Left), 1, 0))
            .action,
        Some(Interaction::ComboBoxDismiss(1))
    );
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

#[test]
fn path_field_routes_popup_keys() {
    let mut form = path_form(3, true);
    assert_eq!(
        form.handle(&key(KeyCode::Down)).action,
        Some(Interaction::Select(1, 1))
    );
    assert_eq!(form.selected(1), Some(1));
    assert_eq!(
        form.handle(&key(KeyCode::Enter)).action,
        Some(Interaction::PathCommit(1, 1))
    );
    assert_eq!(
        form.handle(&key(KeyCode::Esc)).action,
        Some(Interaction::PathDismiss(1))
    );
    assert_eq!(
        form.handle(&key(KeyCode::Tab)).action,
        Some(Interaction::PathDismiss(1))
    );
    assert_eq!(form.focused(), Some(2));
}

#[test]
fn ctrl_space_on_a_path_field_requests_completion() {
    let mut form = path_form(0, false);
    assert_eq!(
        form.handle(&chord(KeyCode::Char(' '), KeyModifiers::CONTROL))
            .action,
        Some(Interaction::Complete(1))
    );
    assert_eq!(
        form.handle(&key(KeyCode::Null)).action,
        Some(Interaction::Complete(1))
    );
}

#[test]
fn collapsed_path_field_edits_like_a_text_field() {
    let mut form = path_form(0, false);
    let typed = key(KeyCode::Char('a'));
    let Event::Key(event) = typed else {
        unreachable!("the fixture makes key events")
    };
    assert_eq!(
        form.handle(&Event::Key(event)).action,
        Some(Interaction::Edit(1, FieldEdit::Key(event)))
    );
    assert_eq!(
        form.handle(&key(KeyCode::Enter)).action,
        Some(Interaction::Activate(1))
    );
}

#[test]
fn popup_click_commits_and_outside_click_dismisses() {
    let mut form = path_form(3, true);
    form.register_popup(
        1,
        Rect::new(0, 1, 12, 5),
        vec![None, Some(0), Some(1), Some(2), None],
    );
    form.handle(&mouse(MouseEventKind::Down(MouseButton::Left), 1, 3));
    assert_eq!(
        form.handle(&mouse(MouseEventKind::Up(MouseButton::Left), 1, 3))
            .action,
        Some(Interaction::PathCommit(1, 1))
    );

    let mut form = path_form(3, true);
    form.register_popup(
        1,
        Rect::new(0, 1, 12, 5),
        vec![None, Some(0), Some(1), Some(2), None],
    );
    assert_eq!(
        form.handle(&mouse(MouseEventKind::Down(MouseButton::Left), 1, 8))
            .action,
        Some(Interaction::PathDismiss(1))
    );
    assert_eq!(form.focused(), Some(2));
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
