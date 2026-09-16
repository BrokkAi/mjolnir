use super::*;

fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

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
fn list_double_click_is_enter_on_the_clicked_item() {
    let mut form = list_form();
    let now = Instant::now();
    assert_eq!(click_at(&mut form, 1, now), Some(Interaction::Select(1, 1)));
    let mut keyboard = form.clone();
    let enter = keyboard.handle(&key(KeyCode::Enter)).action;
    assert_eq!(
        click_at(&mut form, 1, now + Duration::from_millis(200)),
        enter
    );
    assert_eq!(form.selected(1), Some(1));
}

#[test]
fn command_click_activates_and_updates_selection_atomically() {
    let mut form = list_form();
    form.set_list_activation(1, ListActivation::SingleClick);
    assert_eq!(
        click_at(&mut form, 2, Instant::now()),
        Some(Interaction::Activate(1))
    );
    assert_eq!(form.selected(1), Some(2));
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
fn focus_wraps_and_skips_disabled_controls() {
    let mut form = Form::new();
    form.register(1, ControlKind::Button, Rect::new(0, 0, 5, 1), true);
    form.register(2, ControlKind::Button, Rect::new(0, 1, 5, 1), false);
    form.register(3, ControlKind::Button, Rect::new(0, 2, 5, 1), true);
    form.end_frame(1);
    assert_eq!(form.focused(), Some(1));
    assert!(form.handle(&key(KeyCode::Tab)).outcome.is_consumed());
    assert_eq!(form.focused(), Some(3));
    form.handle(&key(KeyCode::Tab));
    assert_eq!(form.focused(), Some(1));
}

#[test]
fn button_releases_activate_once_and_release_outside_cancels() {
    let mut form = Form::new();
    form.register(1, ControlKind::Button, Rect::new(0, 0, 5, 1), true);
    form.end_frame(1);
    let down = Event::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 1,
        row: 0,
        modifiers: KeyModifiers::NONE,
    });
    let up = Event::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Left),
        column: 1,
        row: 0,
        modifiers: KeyModifiers::NONE,
    });
    assert!(form.handle(&down).action.is_none());
    assert!(form.captures_pointer());
    assert_eq!(form.handle(&up).action, Some(Interaction::Activate(1)));
    assert!(!form.captures_pointer());
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
        Outcome::Changed
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
fn requested_initial_focus_and_both_reverse_tab_encodings_work() {
    let mut form = Form::new();
    for id in 1..=3 {
        form.register(id, ControlKind::Button, Rect::default(), true);
    }
    form.end_frame(3);
    assert_eq!(form.focused(), Some(3));
    form.handle(&key(KeyCode::BackTab));
    assert_eq!(form.focused(), Some(2));
    form.handle(&Event::Key(KeyEvent::new(
        KeyCode::Tab,
        KeyModifiers::SHIFT,
    )));
    assert_eq!(form.focused(), Some(1));
    form.handle(&key(KeyCode::BackTab));
    assert_eq!(form.focused(), Some(3));
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
    form.handle(&down);
    assert_eq!(form.focused(), Some(2));
    assert!(
        form.handle(&mouse(MouseEventKind::Up(MouseButton::Left), 20, 20))
            .action
            .is_none()
    );
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
fn disabled_click_is_consumed_without_changing_focus() {
    let mut form = form();
    form.declare_with_enabled(2, ControlKind::Button, false);
    form.end_frame(1);
    let result = form.handle(&mouse(MouseEventKind::Down(MouseButton::Left), 1, 1));
    assert!(result.is_consumed());
    assert_eq!(result.action, None);
    assert_eq!(form.focused(), Some(1));
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
fn activation_keeps_its_action_when_the_form_does_not_repaint() {
    let mut form = form();
    form.focus(2);

    let result = form.handle(&key(KeyCode::Enter));

    assert_eq!(result.outcome, Outcome::Unchanged);
    assert_eq!(result.action, Some(Interaction::Activate(2)));
}

#[test]
fn clamped_navigation_and_repeated_focus_are_consumed_without_changed() {
    let mut form = Form::new();
    form.register(
        1,
        ControlKind::ChoiceList {
            len: 2,
            selected: 1,
        },
        Rect::new(0, 0, 5, 2),
        true,
    );
    form.end_frame(1);

    let result = form.handle(&key(KeyCode::Down));
    assert_eq!(result.outcome, Outcome::Unchanged);
    assert_eq!(result.action, Some(Interaction::Select(1, 1)));

    let result = form.handle(&key(KeyCode::Up));
    assert_eq!(result.outcome, Outcome::Changed);
    assert_eq!(result.action, Some(Interaction::Select(1, 0)));
    assert_eq!(form.selected(1), Some(0));

    let mut only = Form::new();
    only.register(1, ControlKind::Button, Rect::new(0, 0, 5, 1), true);
    only.end_frame(1);
    let result = only.handle(&key(KeyCode::Tab));
    assert_eq!(result.outcome, Outcome::Unchanged);
    assert_eq!(only.focused(), Some(1));
}

#[test]
fn pointer_press_and_release_report_only_the_visible_armed_change() {
    let mut form = form();
    let down = mouse(MouseEventKind::Down(MouseButton::Left), 1, 1);
    let up = mouse(MouseEventKind::Up(MouseButton::Left), 20, 20);

    assert_eq!(form.handle(&down).outcome, Outcome::Changed);
    assert!(form.captures_pointer());
    let result = form.handle(&up);
    assert_eq!(result.outcome, Outcome::Changed);
    assert_eq!(result.action, None);
    assert!(!form.captures_pointer());
}

#[test]
fn disabled_click_and_noop_editing_remain_consumed_without_repaint() {
    let mut form = form();
    form.declare_with_enabled(2, ControlKind::Button, false);
    form.end_frame(1);
    let disabled = form.handle(&mouse(MouseEventKind::Down(MouseButton::Left), 1, 1));
    assert_eq!(disabled.outcome, Outcome::Unchanged);

    let edit = form.handle(&key(KeyCode::Left));
    assert_eq!(edit.outcome, Outcome::Unchanged);
    assert_eq!(
        edit.action,
        Some(Interaction::Edit(
            1,
            FieldEdit::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
        ))
    );
}

#[test]
fn cloned_scopes_do_not_share_focus_flags() {
    let original = form();
    let mut clone = original.clone();
    clone.handle(&key(KeyCode::Tab));
    assert_eq!(clone.focused(), Some(2));
    assert_eq!(original.focused(), Some(1));
    clone.clear();
    assert_eq!(original.focused(), Some(1));
}

#[test]
fn mapped_list_skips_headings_and_disabled_choices_in_keys_and_mouse() {
    let mut form = Form::new();
    form.register_with_rows(
        1,
        ControlKind::ChoiceList {
            len: 4,
            selected: 0,
        },
        Rect::new(0, 0, 12, 4),
        true,
        vec![None, Some(0), Some(1), Some(2)],
        vec![true, true, false, true],
    );
    form.end_frame(1);
    assert_eq!(
        form.handle(&key(KeyCode::Down)).action,
        Some(Interaction::Select(1, 2))
    );
    for y in [0, 2] {
        form.handle(&mouse(MouseEventKind::Down(MouseButton::Left), 1, y));
        assert_eq!(
            form.handle(&mouse(MouseEventKind::Up(MouseButton::Left), 1, y))
                .action,
            None
        );
    }
    form.set_selected(1, 1);
    assert_eq!(form.handle(&key(KeyCode::Enter)).action, None);
    assert_eq!(
        form.handle(&mouse(MouseEventKind::ScrollDown, 1, 1)).action,
        Some(Interaction::Select(1, 2))
    );
}

#[test]
fn dismiss_target_is_not_a_focus_stop_and_cancel_coexists_with_footer_button() {
    let mut form = Form::new();
    form.register(1, ControlKind::Button, Rect::new(10, 0, 8, 1), true);
    form.register_dismiss(Rect::new(1, 0, 3, 1), true);
    form.end_frame(1);
    assert_eq!(form.focused(), Some(1));
    form.handle(&key(KeyCode::Tab));
    assert_eq!(form.focused(), Some(1));

    assert!(
        form.handle(&mouse(MouseEventKind::Down(MouseButton::Left), 2, 0))
            .action
            .is_none()
    );
    assert!(form.dismiss_is_armed());
    assert_eq!(
        form.handle(&mouse(MouseEventKind::Up(MouseButton::Left), 2, 0))
            .action,
        Some(Interaction::Cancel)
    );
    assert!(!form.dismiss_is_armed());
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
fn disabled_dismiss_target_is_visible_to_hit_testing_but_inert() {
    let mut form = Form::<u8>::new();
    form.register_dismiss(Rect::new(1, 0, 3, 1), false);
    form.end_frame(1);
    assert!(form.contains(2, 0));
    let result = form.handle(&mouse(MouseEventKind::Down(MouseButton::Left), 2, 0));
    assert_eq!(result.action, None);
    assert!(!form.captures_pointer());
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
