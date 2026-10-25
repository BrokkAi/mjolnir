use super::*;

#[test]
fn one_click_toggles_a_real_checkbox_and_arrows_only_move_focus() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_settings_section("profiles", None);
    choose(&mut dashboard, "codex-1");
    for expected in [false, true] {
        let lines = drawn(&mut dashboard, 140, 40);
        let (column, row) = point(&lines, "Enabled");
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            dashboard.handle_mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            });
            drawn(&mut dashboard, 140, 40);
        }
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        assert_eq!(dialog.draft["profiles"]["codex-1"]["enabled"], expected);
        assert!(dialog.editor.is_none());
    }
    dashboard.handle_key(key(KeyCode::Down));
    dashboard.handle_key(key(KeyCode::Up));
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert_eq!(dialog.draft["profiles"]["codex-1"]["enabled"], true);
}

/// A row whose value is picked from a list opens its dropdown on one click,
/// and a page button navigates on that same single-click interaction.
#[test]
fn one_click_opens_a_page_and_a_real_dropdown() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    click(&mut dashboard, "Interface");
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert_eq!(dialog.path, ["interface"], "one click opens a page");

    for (section, task) in [("interface", None), ("subagents", Some("straightforward"))] {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.begin_settings_section(section, None);
        if let Some(task) = task {
            choose(&mut dashboard, task);
        }
        let label = if task.is_some() { "Model" } else { "Theme" };
        let lines = drawn(&mut dashboard, 140, 40);
        let (_, row) = point(&lines, label);
        let column = cell_column(&lines[usize::from(row)], "▾");
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            dashboard.handle_mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            });
            drawn(&mut dashboard, 140, 40);
        }
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        assert!(
            dialog.combo.open_id().is_some(),
            "one click opens the dropdown"
        );
    }
}

#[test]
fn choice_popup_escape_preserves_the_draft_and_background_click_is_inert() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    choose(&mut dashboard, "interface");
    choose(&mut dashboard, "theme");
    dashboard.handle_key(key(KeyCode::Down));
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .unwrap();

    // A click away from the popup must not activate the inert mirror of the
    // page's stacked controls or commit the pending choice.
    let (row, column) = buffer_lines(terminal.backend().buffer())
        .iter()
        .enumerate()
        .find_map(|(row, line)| line.find("Save and Close").map(|column| (row, column)))
        .expect("mirrored Save row");
    let background_button = (column as u16 + 1, row as u16);
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        dashboard.handle_event_result(Event::Mouse(MouseEvent {
            kind,
            column: background_button.0,
            row: background_button.1,
            modifiers: KeyModifiers::NONE,
        }));
    }
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings");
    };
    assert!(dialog.combo.open_id().is_some());
    assert_eq!(dialog.draft["theme"], "midnight");
    dashboard.handle_key(key(KeyCode::Esc));
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings");
    };
    assert!(dialog.combo.open_id().is_none(), "escape closes the popup");
    assert_eq!(dialog.draft["theme"], "midnight");

    // Reopen it for the pointer-commit part of the behavior.
    choose(&mut dashboard, "theme");
    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .unwrap();

    // Find one popup content cell from the rendered form and click it.
    let (row, column) = buffer_lines(terminal.backend().buffer())
        .iter()
        .enumerate()
        .find_map(|(row, line)| line.find("Light").map(|column| (row, column)))
        .expect("Light popup row");
    let point = (column as u16, row as u16);
    assert!(
        setup_dialog_mut(&mut dashboard.mode)
            .is_some_and(|dialog| dialog.form.borrow().contains(point.0, point.1))
    );
    dashboard.handle_event_result(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: point.0,
        row: point.1,
        modifiers: KeyModifiers::NONE,
    }));
    dashboard.handle_event_result(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Left),
        column: point.0,
        row: point.1,
        modifiers: KeyModifiers::NONE,
    }));
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings");
    };
    assert!(
        dialog.combo.open_id().is_none(),
        "click commits the popup choice"
    );
    assert_eq!(dialog.draft["theme"], "light");
}

#[test]
fn cancelling_or_failing_to_save_a_theme_keeps_the_active_colors() {
    let mut dashboard = dashboard_with_session(stopped_session());
    let original = dashboard.config.clone();
    choose_light_theme(&mut dashboard);
    dashboard.handle_key(key(KeyCode::Esc));
    assert!(dashboard.modal_open());
    dashboard.handle_key(key(KeyCode::Right));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert!(!dashboard.modal_open());
    assert_eq!(dashboard.config, original);
    assert_rendered_theme(&mut dashboard, theme::UiTheme::Midnight);

    choose_light_theme(&mut dashboard);
    let action = dashboard.handle_key(crossterm::event::KeyEvent::new(
        KeyCode::Char('s'),
        KeyModifiers::CONTROL,
    ));
    let DashboardAction::SaveSetup { generation, .. } = action else {
        panic!("{action:?}");
    };
    dashboard.setup_saved(generation, Err("disk full".into()));
    assert_eq!(dashboard.config, original);
    assert_rendered_theme(&mut dashboard, theme::UiTheme::Midnight);
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert_eq!(dialog.draft["theme"], "light");
    assert!(dialog.notice.as_ref().unwrap().contains("disk full"));
}

#[test]
fn backspace_at_the_root_with_a_dirty_draft_asks_before_discarding() {
    let mut dashboard = dashboard_with_session(stopped_session());
    let original = dashboard.config.clone();
    dashboard.begin_setup();
    choose(&mut dashboard, "phone");
    choose(&mut dashboard, "enabled");
    dashboard.handle_key(key(KeyCode::Backspace));
    dashboard.handle_key(key(KeyCode::Backspace));
    let Mode::Confirm(_) = &dashboard.mode else {
        panic!(
            "dirty settings must confirm before closing: {:?}",
            dashboard.modal_open()
        );
    };
    // Esc keeps editing with the draft intact.
    dashboard.handle_key(key(KeyCode::Esc));
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings restored");
    };
    assert!(dialog.is_dirty());
    assert_eq!(dashboard.config, original);
}

#[test]
fn cancelling_setup_preserves_configuration_and_render_keeps_controls_visible() {
    let mut dashboard = dashboard_with_session(stopped_session());
    let original = dashboard.config.clone();
    for (width, height) in [(80, 18), (100, 30), (140, 42)] {
        dashboard.begin_setup();
        choose(&mut dashboard, "phone");
        choose(&mut dashboard, "enabled");
        dashboard.handle_key(key(KeyCode::Backspace));
        choose(&mut dashboard, "targets");
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, &mut dashboard))
            .unwrap();
        let lines = buffer_lines(terminal.backend().buffer());
        let text = lines.join("\n");
        assert!(text.contains("Settings › Runtimes"), "{text}");
        // Every page action keeps its own row in one column at the dialog's
        // right edge, on top of each other rather than spread along a row.
        let labels = ["Add", "Detect runtimes"];
        let width = labels
            .iter()
            .map(|label| label.len())
            .max()
            .expect("labels");
        let mut rows = Vec::new();
        for label in labels {
            // The button's padding separates it from prose that happens to
            // use the same word, such as the page's help line.
            let padded = format!("  {label}  ");
            let (row, line) = lines
                .iter()
                .enumerate()
                .find(|(_, line)| line.contains(&padded))
                .unwrap_or_else(|| panic!("missing {label:?} in\n{text}"));
            // Every button is as wide as the longest of them, so a shorter
            // label is followed by its share of that width, the button's
            // padding, the inner margin, and then the modal border.
            let after = &line[line.find(&padded).unwrap() + 2 + label.len()..];
            let gap = format!("{}│", " ".repeat(3 + width - label.len()));
            assert!(
                after.starts_with(&gap),
                "{label} is not packed against the dialog's right edge: {line}"
            );
            rows.push((row, cell_column(line, &padded) + 2, label));
        }
        assert!(
            rows.windows(2).all(|pair| pair[0].0 < pair[1].0),
            "the buttons are not stacked in order: {rows:?}\n{text}"
        );
        let (_, first_column, _) = rows[0];
        assert!(
            rows.iter().all(|(_, column, _)| *column == first_column),
            "the stacked buttons do not share a column: {rows:?}\n{text}"
        );
        assert!(
            rows.windows(2).all(|pair| pair[0].0 + 1 == pair[1].0),
            "the stacked buttons leave gaps between them: {rows:?}\n{text}"
        );
        // Back and the commit share the footer row below the column.
        let (back_column, back_row) = point(&lines, "  Back  ");
        let (save_column, save_row) = point(&lines, "  Save and Close  ");
        assert_eq!(back_row, save_row, "{text}");
        assert!(back_column < save_column, "{text}");
        assert!(
            rows.iter().all(|(row, _, _)| (*row as u16) < back_row),
            "the column overlaps the footer row: {rows:?}\n{text}"
        );
        assert!(!text.contains("Cancel"), "{text}");
        // Backspace from the root dismisses through the dirty guard.
        dashboard.handle_key(key(KeyCode::Backspace));
        dashboard.handle_key(key(KeyCode::Backspace));
        assert!(matches!(dashboard.mode, Mode::Confirm(_)), "{text}");
        dashboard.handle_key(key(KeyCode::Right));
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::None
        );
        assert!(!dashboard.modal_open());
        assert_eq!(dashboard.config, original);
    }
}

/// The cache size is typed, stored and shown as a whole number of GB.
/// The saved file keeps only the build cache fields that are set, so the page
/// has to fill the rest back in: all three stay listed and editable after a
/// save, and each one can still be handed back to the host.
/// The border promises `Esc back`, and below the first page that is what it
/// must do: return to the parent with the draft intact.
// Hard-won: 0cc7c1e6: Escape on a subpage discarded the entire settings draft.
#[test]
fn escape_returns_from_a_settings_subpage_and_closes_only_from_the_first_page() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    choose(&mut dashboard, "machines");
    choose(&mut dashboard, "local");
    dashboard.handle_key(key(KeyCode::Esc));
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("Esc on a sub-page must keep Settings open");
    };
    assert_eq!(dialog.path, ["machines"]);
    dashboard.handle_key(key(KeyCode::Esc));
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("Esc on the Machines page must return to the first page");
    };
    assert!(dialog.path.is_empty());

    // A sub-page edit survives the way back out.
    choose(&mut dashboard, "phone");
    choose(&mut dashboard, "enabled");
    dashboard.handle_key(key(KeyCode::Esc));
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("Esc must not discard the draft from a sub-page");
    };
    assert!(dialog.path.is_empty());
    assert_eq!(dialog.draft["phone"]["enabled"], Value::Bool(false));
    assert!(dialog.is_dirty());

    // From the first page Esc closes the dialog, through the discard guard.
    dashboard.handle_key(key(KeyCode::Esc));
    assert!(
        matches!(dashboard.mode, Mode::Confirm(_)),
        "a dirty draft asks before closing"
    );
}

/// A status line belongs to the page or prompt that set it. Finding T-4.
// Hard-won: 6094bb1c: a prompt status line remained after its owning settings page closed.
#[test]
fn a_status_line_does_not_outlive_the_prompt_that_set_it() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_settings_section("profiles", None);
    dashboard.handle_key(key(KeyCode::Char('a')));
    dashboard.handle_key(key(KeyCode::Enter));
    let shown = setup_dialog_mut(&mut dashboard.mode)
        .unwrap()
        .notice
        .clone();
    assert_eq!(shown.as_deref(), Some("Choose a name for the new entry."));
    dashboard.handle_key(key(KeyCode::Esc));
    choose(&mut dashboard, "codex-1");
    choose(&mut dashboard, "environment");
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert_eq!(dialog.notice, None, "the name prompt's status lingered");
    let page = drawn(&mut dashboard, 140, 40).join("\n");
    assert!(!page.contains("Choose a name"), "{page}");
}

#[tokio::test]
async fn settings_owns_the_terminal_cursor_and_restores_the_composer_after_dismissal() {
    use mj_chat::chat::{ActiveChat, Notices, SessionHeaderIdentity};
    let session = running_session();
    let id = session.id.clone();
    let mut dashboard = dashboard_with_session(session);
    dashboard.focus_prompt();
    let fixture = mj_client::session::replacement_session_test_fixture(&id, 1);
    let chat = ActiveChat::open(
        fixture.stopped,
        "hel",
        None,
        fixture.control,
        SessionHeaderIdentity::default(),
        String::new(),
        Notices::default(),
    );
    let mut chats = std::collections::BTreeMap::from([(id, chat)]);
    use ratatui::{TerminalOptions, Viewport, backend::CrosstermBackend};
    #[derive(Clone, Default)]
    struct Output(std::rc::Rc<RefCell<Vec<u8>>>);
    impl std::io::Write for Output {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let output = Output::default();
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(output.clone()),
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, 140, 40)),
        },
    )
    .unwrap();
    let draw = |terminal: &mut Terminal<CrosstermBackend<Output>>,
                dashboard: &mut DashboardState,
                chats: &mut std::collections::BTreeMap<String, ActiveChat>| {
        output.0.borrow_mut().clear();
        terminal
            .draw(|frame| {
                crate::combined::render_combined_for_test(
                    frame,
                    dashboard,
                    chats,
                    &Default::default(),
                    false,
                );
            })
            .unwrap();
    };
    draw(&mut terminal, &mut dashboard, &mut chats);
    assert!(
        output
            .0
            .borrow()
            .windows(6)
            .any(|bytes| bytes == b"\x1b[?25h"),
        "composer owns the initial cursor"
    );
    dashboard.begin_settings_section("notify", None);
    draw(&mut terminal, &mut dashboard, &mut chats);
    assert!(
        output
            .0
            .borrow()
            .windows(6)
            .any(|bytes| bytes == b"\x1b[?25l"),
        "settings controls cannot inherit the background cursor"
    );
    choose(&mut dashboard, "delay_seconds");
    draw(&mut terminal, &mut dashboard, &mut chats);
    assert!(
        output
            .0
            .borrow()
            .windows(6)
            .any(|bytes| bytes == b"\x1b[?25h"),
        "focused modal text field owns its cursor"
    );
    dashboard.handle_key(key(KeyCode::Esc));
    draw(&mut terminal, &mut dashboard, &mut chats);
    assert!(
        output
            .0
            .borrow()
            .windows(6)
            .any(|bytes| bytes == b"\x1b[?25l"),
        "closing the editor hides its cursor"
    );
    assert!(
        matches!(&dashboard.mode, Mode::Setup(dialog) if dialog.path.is_empty()),
        "Esc returns to the Settings root"
    );
    dashboard.handle_key(key(KeyCode::Esc));
    draw(&mut terminal, &mut dashboard, &mut chats);
    assert!(
        output
            .0
            .borrow()
            .windows(6)
            .any(|bytes| bytes == b"\x1b[?25h"),
        "dismissal restores composer cursor"
    );
}

// Hard-won: Settings review: input preparation discarded the rendered viewport.
#[test]
fn wheel_scrolls_settings_body_without_moving_focus() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    let before = drawn(&mut dashboard, 100, 20);
    let (column, row) = point(&before, "Profiles");
    let focus = setup_dialog_mut(&mut dashboard.mode)
        .unwrap()
        .focused_path();
    for _ in 0..5 {
        dashboard.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        });
        drawn(&mut dashboard, 100, 20);
    }
    let after = drawn(&mut dashboard, 100, 20);
    assert_ne!(after, before, "wheel must move the scrolling body");
    assert!(
        !after.iter().any(|line| line.contains("Agent Profiles")),
        "the first page row must scroll out of view: {after:#?}"
    );
    assert_eq!(
        setup_dialog_mut(&mut dashboard.mode)
            .unwrap()
            .focused_path(),
        focus
    );
}
