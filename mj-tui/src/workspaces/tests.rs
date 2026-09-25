use super::*;
use crate::test_support::{
    buffer_lines, cell_column, chord, dashboard_with_session, drawn, running_session,
};
use crossterm::event::{KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use mj_core::workspace::WorkspaceRecord;

fn entry(id: &str, name: &str) -> WorkspaceManagementEntry {
    WorkspaceManagementEntry {
        workspace: WorkspaceRecord {
            id: id.into(),
            name: name.into(),
            created_at: String::new(),
            last_opened_at: String::new(),
            session_count: 0,
        },
        drafts: Vec::new(),
    }
}

fn draw_manager(dashboard: &DashboardState) -> Vec<String> {
    use ratatui::{Terminal, backend::TestBackend};
    let mut terminal = Terminal::new(TestBackend::new(100, 28)).unwrap();
    let mut surfaces = FrameSurfaces::new();
    terminal
        .draw(|frame| {
            if let Mode::WorkspaceManager(manager) = &dashboard.mode {
                render_workspace_manager(frame, frame.area(), manager, &mut surfaces);
            }
        })
        .unwrap();
    buffer_lines(terminal.backend().buffer())
}

fn manager_click(dashboard: &mut DashboardState, lines: &[String], label: &str) {
    let (row, line) = lines
        .iter()
        .enumerate()
        .find(|(_, line)| line.contains(label))
        .unwrap_or_else(|| panic!("missing {label:?}: {lines:#?}"));
    let column = cell_column(line, label) + 1;
    let mouse = |kind| MouseEvent {
        kind,
        column,
        row: row as u16,
        modifiers: KeyModifiers::NONE,
    };
    assert_eq!(
        dashboard.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left))),
        DashboardAction::None
    );
    assert_eq!(
        dashboard.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left))),
        DashboardAction::None
    );
}

#[test]
fn manager_ignores_stale_results_and_selects_workspace_on_enter() {
    let mut dashboard = dashboard_with_session(running_session());
    let load = dashboard.begin_workspace_manager();
    let DashboardAction::LoadWorkspaceManagement { generation } = load else {
        panic!("manager did not request a load");
    };
    dashboard.finish_workspace_management(
        generation.wrapping_sub(1),
        Ok(vec![entry("stale", "Stale")]),
    );
    assert!(matches!(
        &dashboard.mode,
        Mode::WorkspaceManager(manager) if manager.loading
    ));
    dashboard.finish_workspace_management(generation, Ok(vec![entry("other", "Other")]));
    assert!(matches!(
        &dashboard.mode,
        Mode::WorkspaceManager(manager) if !manager.loading
    ));
    let action = dashboard.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        action,
        DashboardAction::SelectWorkspace {
            workspace_id: "other".into()
        }
    );
}

#[test]
fn tabs_scroll_to_the_selected_workspace_and_mouse_hits_unicode_labels() {
    use ratatui::{Terminal, backend::TestBackend};
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.set_workspace_names(std::collections::BTreeMap::from([
        ("a".into(), "First workspace".into()),
        ("b".into(), "界界".into()),
        ("c".into(), "Last workspace".into()),
    ]));
    dashboard.set_active_workspace(Some("b".into()));
    let mut terminal = Terminal::new(TestBackend::new(24, 3)).unwrap();
    terminal
        .draw(|frame| render_workspace_tabs(frame, Rect::new(0, 0, 24, 3), &mut dashboard))
        .unwrap();
    assert_eq!(
        dashboard.workspace_tab_areas[0],
        ("b".into(), Rect::new(1, 1, 6, 1))
    );
    let buffer = terminal.backend().buffer();
    assert_eq!(buffer[(1, 1)].symbol(), " ");
    assert_eq!(buffer[(2, 1)].symbol(), "界");
    assert_eq!(buffer[(4, 1)].symbol(), "界");
    assert_eq!(buffer[(6, 1)].symbol(), " ");
    assert_eq!(buffer[(1, 1)].bg, buffer[(6, 1)].bg);
    assert!(
        matches!(workspace_tab_click(&mut dashboard, 8, 1), Some(DashboardAction::SelectWorkspace { workspace_id }) if workspace_id == "c")
    );
    assert_eq!(dashboard.focus(), crate::Focus::Workspaces);
    dashboard.set_active_workspace(Some("c".into()));
    terminal
        .draw(|frame| render_workspace_tabs(frame, Rect::new(0, 0, 24, 3), &mut dashboard))
        .unwrap();
    assert!(
        dashboard
            .workspace_tab_areas
            .iter()
            .any(|(id, _)| id == "c")
    );
    let text = (0..24)
        .map(|x| terminal.backend().buffer()[(x, 1)].symbol())
        .collect::<String>();
    assert!(text.contains("Last workspace"), "{text}");
}

#[test]
fn manager_initial_selection_follows_the_current_tab() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.set_active_workspace(Some("b".into()));
    let DashboardAction::LoadWorkspaceManagement { generation } =
        dashboard.begin_workspace_manager()
    else {
        panic!("load");
    };
    dashboard.finish_workspace_management(generation, Ok(vec![entry("a", "A"), entry("b", "B")]));
    assert!(
        matches!(dashboard.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), DashboardAction::SelectWorkspace { workspace_id } if workspace_id == "b")
    );
}

#[test]
fn hamburger_is_pinned_and_opens_manager_on_mouse_release() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    use ratatui::{Terminal, backend::TestBackend};

    let mut dashboard = dashboard_with_session(running_session());
    dashboard.workspace_names.insert(
        mj_core::workspace::DEFAULT_WORKSPACE_ID.into(),
        "A very long workspace name".into(),
    );
    let mut terminal = Terminal::new(TestBackend::new(24, 3)).unwrap();
    terminal
        .draw(|frame| render_workspace_tabs(frame, frame.area(), &mut dashboard))
        .unwrap();
    let buffer = terminal.backend().buffer();
    assert_eq!(buffer[(20, 1)].symbol(), " ");
    assert_eq!(buffer[(21, 1)].symbol(), "☰");
    assert_eq!(buffer[(22, 1)].symbol(), " ");
    assert!(dashboard.surface_form.borrow().contains(21, 1));

    let mouse = |kind| MouseEvent {
        kind,
        column: 21,
        row: 1,
        modifiers: KeyModifiers::NONE,
    };
    assert_eq!(
        dashboard.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left))),
        DashboardAction::None
    );
    assert!(matches!(
        dashboard.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left))),
        DashboardAction::LoadWorkspaceManagement { generation: 1 }
    ));
    assert!(matches!(dashboard.mode, Mode::WorkspaceManager(_)));
}

#[test]
fn workspace_menu_has_a_keyboard_stop_before_sessions() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.focus = crate::Focus::Workspaces;
    assert_eq!(
        dashboard.workspace_control_focus,
        WorkspaceControlFocus::Tabs
    );
    assert_eq!(
        dashboard.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
        DashboardAction::None
    );
    assert_eq!(
        dashboard.workspace_control_focus,
        WorkspaceControlFocus::Menu
    );
    assert_eq!(
        dashboard.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
        DashboardAction::None
    );
    assert_eq!(dashboard.focus, crate::Focus::Sessions);

    dashboard.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
    assert_eq!(dashboard.focus, crate::Focus::Workspaces);
    assert_eq!(
        dashboard.workspace_control_focus,
        WorkspaceControlFocus::Menu
    );
    dashboard.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
    assert_eq!(dashboard.focus, crate::Focus::Workspaces);
    assert_eq!(
        dashboard.workspace_control_focus,
        WorkspaceControlFocus::Tabs
    );

    dashboard.focus = crate::Focus::Workspaces;
    dashboard.workspace_control_focus = WorkspaceControlFocus::Menu;
    assert_eq!(
        dashboard.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        DashboardAction::LoadWorkspaceManagement { generation: 1 }
    );
}

#[test]
fn vertical_arrows_mirror_left_and_right_on_the_tabs() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.set_workspace_names(std::collections::BTreeMap::from([
        ("a".into(), "First".into()),
        ("b".into(), "Second".into()),
        ("c".into(), "Last".into()),
    ]));
    dashboard.set_active_workspace(Some("b".into()));
    dashboard.focus = crate::Focus::Workspaces;
    assert_eq!(
        dashboard.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        DashboardAction::SelectWorkspace {
            workspace_id: "c".into()
        }
    );
    assert_eq!(
        dashboard.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        DashboardAction::SelectWorkspace {
            workspace_id: "a".into()
        }
    );
    // Down at the last tab hands the pane to the pinned menu, like Right.
    dashboard.set_active_workspace(Some("c".into()));
    assert_eq!(
        dashboard.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        DashboardAction::None
    );
    assert_eq!(
        dashboard.workspace_control_focus,
        WorkspaceControlFocus::Menu
    );
    // Up from the menu returns to the tabs, like Left.
    assert_eq!(
        dashboard.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        DashboardAction::None
    );
    assert_eq!(
        dashboard.workspace_control_focus,
        WorkspaceControlFocus::Tabs
    );
}

#[test]
fn mouse_wheel_over_the_workspace_pane_switches_tabs_without_focus() {
    use ratatui::{Terminal, backend::TestBackend};

    let mut dashboard = dashboard_with_session(running_session());
    dashboard.set_workspace_names(std::collections::BTreeMap::from([
        ("a".into(), "First".into()),
        ("b".into(), "Second".into()),
        ("c".into(), "Last".into()),
    ]));
    dashboard.set_active_workspace(Some("b".into()));
    dashboard.focus = crate::Focus::Sessions;
    let mut terminal = Terminal::new(TestBackend::new(24, 3)).unwrap();
    terminal
        .draw(|frame| render_workspace_tabs(frame, Rect::new(0, 0, 24, 3), &mut dashboard))
        .unwrap();
    let wheel = |kind| MouseEvent {
        kind,
        column: 2,
        row: 1,
        modifiers: KeyModifiers::NONE,
    };
    assert_eq!(
        dashboard.handle_mouse(wheel(MouseEventKind::ScrollDown)),
        DashboardAction::SelectWorkspace {
            workspace_id: "c".into()
        }
    );
    assert_eq!(
        dashboard.focus,
        crate::Focus::Sessions,
        "the wheel switches tabs without stealing focus"
    );
    dashboard.set_active_workspace(Some("c".into()));
    assert_eq!(
        dashboard.handle_mouse(wheel(MouseEventKind::ScrollUp)),
        DashboardAction::SelectWorkspace {
            workspace_id: "b".into()
        }
    );
}

#[test]
fn manager_views_keep_rename_identity_across_reordered_refresh() {
    let mut dashboard = dashboard_with_session(running_session());
    let DashboardAction::LoadWorkspaceManagement { generation } =
        dashboard.begin_workspace_manager()
    else {
        panic!("load");
    };
    dashboard.finish_workspace_management(generation, Ok(vec![entry("a", "A"), entry("b", "B")]));
    if let Mode::WorkspaceManager(manager) = &mut dashboard.mode {
        manager.form.get_mut().focus(WorkspaceControl::Rename);
    }
    assert_eq!(
        dashboard.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        DashboardAction::None
    );
    assert!(matches!(
        &dashboard.mode,
        Mode::WorkspaceManager(manager)
            if matches!(&manager.view, WorkspaceManagerView::Rename { workspace_id } if workspace_id == "a")
    ));
    dashboard.finish_workspace_management(
        generation,
        Ok(vec![entry("b", "B"), entry("a", "A renamed")]),
    );
    assert!(matches!(
        &dashboard.mode,
        Mode::WorkspaceManager(manager)
            if matches!(&manager.view, WorkspaceManagerView::Rename { workspace_id } if workspace_id == "a")
    ));
}

#[test]
fn manager_actions_activate_on_mouse_release() {
    let mut dashboard = dashboard_with_session(running_session());
    let DashboardAction::LoadWorkspaceManagement { generation } =
        dashboard.begin_workspace_manager()
    else {
        panic!("load");
    };
    dashboard.finish_workspace_management(generation, Ok(vec![entry("a", "A")]));
    let lines = draw_manager(&dashboard);

    manager_click(&mut dashboard, &lines, "Rename");

    assert!(matches!(
        &dashboard.mode,
        Mode::WorkspaceManager(manager)
            if matches!(&manager.view, WorkspaceManagerView::Rename { workspace_id } if workspace_id == "a")
    ));
}

#[test]
fn delete_with_active_sessions_uses_the_cancellable_suspension_workflow() {
    let mut dashboard = dashboard_with_session(running_session());
    let DashboardAction::LoadWorkspaceManagement { generation } =
        dashboard.begin_workspace_manager()
    else {
        panic!("load");
    };
    let mut busy_entry = entry("a", "Project alpha");
    busy_entry.workspace.session_count = 2;
    busy_entry.drafts.push(WorkspaceDraftEntry {
        id: "draft-a".into(),
        session_id: Some("session-a".into()),
        source: "composer".into(),
        saved_at: "now".into(),
        owner_pid: None,
    });
    dashboard.finish_workspace_management(generation, Ok(vec![busy_entry]));
    if let Mode::WorkspaceManager(manager) = &mut dashboard.mode {
        manager.form.get_mut().focus(WorkspaceControl::Delete);
    }
    dashboard.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(
        &dashboard.mode,
        Mode::WorkspaceManager(manager) if matches!(&manager.view, WorkspaceManagerView::Close { .. })
    ));
    let rendered = draw_manager(&dashboard).join("\n");
    assert!(rendered.contains("Suspend 2 sessions"), "{rendered}");
    assert!(rendered.contains("Discard 1 saved draft and"), "{rendered}");
    assert!(!rendered.contains("Force delete"), "{rendered}");
    if let Mode::WorkspaceManager(manager) = &mut dashboard.mode {
        manager.form.get_mut().focus(WorkspaceControl::ConfirmClose);
    }
    assert_eq!(
        dashboard.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        DashboardAction::CloseWorkspace {
            generation,
            workspace_id: "a".into(),
        }
    );
}

#[test]
fn the_list_view_dismisses_from_its_title_bar_rather_than_a_close_button() {
    let mut dashboard = dashboard_with_session(running_session());
    let DashboardAction::LoadWorkspaceManagement { generation } =
        dashboard.begin_workspace_manager()
    else {
        panic!("load");
    };
    dashboard
        .finish_workspace_management(generation, Ok(vec![entry("workspace-a", "Project alpha")]));
    let lines = draw_manager(&dashboard);
    let list = lines.join("\n");
    // Deleting a workspace is an explicit action, distinct from dismissing
    // the manager through its title-bar ×.
    assert!(list.contains("Delete…"), "{list}");
    assert!(!list.contains("Force delete"), "{list}");
    assert!(list.contains('×'), "{list}");
    // The actions stack in a column at the dialog's right edge, one per
    // row, in the order they apply.
    let labels = ["New workspace", "Rename", "Delete…", "Open"];
    let width = labels
        .iter()
        .map(|label| label.chars().count())
        .max()
        .expect("labels");
    let mut rows = Vec::new();
    for label in labels {
        let (row, line) = lines
            .iter()
            .enumerate()
            .find(|(_, line)| line.contains(label))
            .unwrap_or_else(|| panic!("missing {label:?} in\n{list}"));
        // Every button shares the widest label's width, so a shorter label
        // is followed by its share of that width and the button's padding,
        // and nothing else before the dialog's right edge.
        let after = &line[line.find(label).unwrap() + label.len()..];
        let gap = format!("{}│", " ".repeat(2 + width - label.chars().count()));
        assert!(
            after.starts_with(&gap),
            "{label} is not packed against the dialog's right edge: {line}"
        );
        rows.push((row, cell_column(line, label), label));
    }
    assert!(
        rows.windows(2).all(|pair| pair[0].0 + 1 == pair[1].0),
        "the buttons are not stacked in order: {rows:?}\n{list}"
    );
    let (_, first_column, _) = rows[0];
    assert!(
        rows.iter().all(|(_, column, _)| *column == first_column),
        "the stacked buttons do not share a column: {rows:?}\n{list}"
    );

    // Both remaining dismiss paths work: the title bar's × by mouse, and
    // Esc by keyboard.
    manager_click(&mut dashboard, &lines, "×");
    assert!(!matches!(dashboard.mode, Mode::WorkspaceManager(_)));

    let DashboardAction::LoadWorkspaceManagement { generation } =
        dashboard.begin_workspace_manager()
    else {
        panic!("load");
    };
    dashboard
        .finish_workspace_management(generation, Ok(vec![entry("workspace-a", "Project alpha")]));
    let _ = draw_manager(&dashboard);
    assert_eq!(
        dashboard.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        DashboardAction::None
    );
    assert!(!matches!(dashboard.mode, Mode::WorkspaceManager(_)));
}

#[test]
fn manager_renders_distinct_create_and_drafts_views() {
    let mut dashboard = dashboard_with_session(running_session());
    let DashboardAction::LoadWorkspaceManagement { generation } =
        dashboard.begin_workspace_manager()
    else {
        panic!("load");
    };
    let mut workspace = entry("workspace-a", "Project alpha");
    workspace.drafts.push(WorkspaceDraftEntry {
        id: "draft-a".into(),
        session_id: Some("session-a".into()),
        source: "composer".into(),
        saved_at: "today".into(),
        owner_pid: None,
    });
    dashboard.finish_workspace_management(generation, Ok(vec![workspace]));
    let lines = draw_manager(&dashboard);
    let list = lines.join("\n");
    assert!(list.contains("New workspace"), "{list}");
    assert!(list.contains("Open"), "{list}");
    assert!(list.contains("Drafts"), "{list}");
    // Create belongs to the column the list is set beside, not to a lone
    // button above the list where it reads as part of the dialog's header.
    let list_row = lines
        .iter()
        .position(|line| line.contains("Project alpha"))
        .expect("the list row");
    let actions = lines
        .iter()
        .position(|line| line.contains("New workspace"))
        .expect("the action column");
    assert!(actions >= list_row, "{list}");

    if let Mode::WorkspaceManager(manager) = &mut dashboard.mode {
        manager.form.get_mut().focus(WorkspaceControl::New);
    }
    dashboard.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let create = draw_manager(&dashboard).join("\n");
    assert!(create.contains("Workspaces · New"), "{create}");
    assert!(create.contains("Create"), "{create}");
    assert!(create.contains("Cancel"), "{create}");
    if let Mode::WorkspaceManager(manager) = &mut dashboard.mode {
        manager.form.get_mut().focus(WorkspaceControl::Back);
    }
    assert_eq!(
        dashboard.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        DashboardAction::None
    );
    let _ = draw_manager(&dashboard);

    if let Mode::WorkspaceManager(manager) = &mut dashboard.mode {
        manager.form.get_mut().focus(WorkspaceControl::Drafts);
    }
    dashboard.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let drafts = draw_manager(&dashboard).join("\n");
    assert!(drafts.contains("Workspaces · Drafts"), "{drafts}");
    assert!(drafts.contains("composer"), "{drafts}");
    assert!(drafts.contains("Recover"), "{drafts}");
    assert!(drafts.contains("Back"), "{drafts}");
}

#[test]
fn manager_load_finishes_behind_help_and_is_restored_when_help_closes() {
    let mut dashboard = dashboard_with_session(running_session());
    let DashboardAction::LoadWorkspaceManagement { generation } =
        dashboard.begin_workspace_manager()
    else {
        panic!("manager did not request a load");
    };
    assert_eq!(
        dashboard.dispatch_command(crate::CommandId::Help),
        DashboardAction::None
    );
    assert!(matches!(&dashboard.mode, Mode::Help(_)));

    assert!(!dashboard.finish_workspace_management(
        generation,
        Ok(vec![entry("workspace-a", "Workspace A")]),
    ));
    assert!(matches!(
        &dashboard.mode,
        Mode::Help(overlay)
            if matches!(overlay.return_to.as_ref(), Mode::WorkspaceManager(manager) if !manager.loading)
    ));

    assert_eq!(
        chord(&mut dashboard, crate::CommandId::Help),
        DashboardAction::None
    );
    assert!(matches!(
        &dashboard.mode,
        Mode::WorkspaceManager(manager)
            if !manager.loading && manager.entries[0].workspace.id == "workspace-a"
    ));
}

/// The build has to be readable from the dashboard itself. A sidebar too
/// narrow for both titles drops the version instead of overwriting the pane's
/// own title.
#[test]
fn workspace_pane_names_the_running_version_until_the_sidebar_is_too_narrow() {
    let mut dashboard = dashboard_with_session(running_session());
    let version = format!("v{}", env!("CARGO_PKG_VERSION"));

    let wide = drawn(&mut dashboard, 120, 40).join("\n");
    assert!(wide.contains("Workspaces"), "{wide}");
    assert!(wide.contains(&version), "{wide}");

    dashboard.set_pane_size(crate::SupportPane::Sessions, crate::PaneSize::Minimized);
    let narrow = drawn(&mut dashboard, 120, 40).join("\n");
    assert!(narrow.contains("Workspac"), "{narrow}");
    assert!(!narrow.contains(&version), "{narrow}");
}

#[test]
fn workspace_shortcuts_load_the_active_workspace_and_ignore_stale_replies() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.set_active_workspace(Some("b".into()));
    let DashboardAction::LoadWorkspaceManagement { generation } =
        chord(&mut dashboard, crate::CommandId::RenameWorkspace)
    else {
        panic!("load");
    };
    dashboard
        .finish_workspace_management(generation.wrapping_sub(1), Ok(vec![entry("b", "Wrong")]));
    assert!(matches!(&dashboard.mode, Mode::WorkspaceManager(manager) if manager.loading));
    dashboard.finish_workspace_management(
        generation,
        Ok(vec![entry("a", "Other"), entry("b", "Rename me")]),
    );
    assert!(
        matches!(&dashboard.mode, Mode::WorkspaceManager(manager) if matches!(&manager.view, WorkspaceManagerView::Rename { workspace_id } if workspace_id == "b") && manager.name.value() == "Rename me")
    );
    if let Mode::WorkspaceManager(manager) = &mut dashboard.mode {
        manager.name = TextInput::from_value("New name");
    }
    assert_eq!(
        dashboard.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        DashboardAction::RenameWorkspace {
            generation,
            workspace_id: "b".into(),
            name: "New name".into()
        }
    );
}

#[test]
fn closing_workspace_confirms_counts_and_supports_cancellation_while_busy() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.set_active_workspace(Some("a".into()));
    let DashboardAction::LoadWorkspaceManagement { generation } =
        chord(&mut dashboard, crate::CommandId::CloseWorkspace)
    else {
        panic!("load");
    };
    let mut workspace = entry("a", "Example");
    workspace.workspace.session_count = 2;
    dashboard.finish_workspace_management(generation, Ok(vec![workspace]));
    let rendered = draw_manager(&dashboard).join("\n");
    // Launch finding R5-10: the counts agree with their nouns, not "(s)".
    assert!(rendered.contains("Suspend 2 sessions,"), "{rendered}");
    assert!(rendered.contains("Resumable histories"));
    assert!(rendered.contains("Discard 0 saved drafts"), "{rendered}");
    assert!(!rendered.contains("(s)"), "{rendered}");
    assert_eq!(
        dashboard.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        DashboardAction::None
    );
    // Cancel leaves the dialog for the dashboard the shortcut came from.
    assert!(matches!(&dashboard.mode, Mode::Dashboard));
    let DashboardAction::LoadWorkspaceManagement { generation } =
        chord(&mut dashboard, crate::CommandId::CloseWorkspace)
    else {
        panic!("load");
    };
    let mut workspace = entry("a", "Example");
    workspace.workspace.session_count = 2;
    dashboard.finish_workspace_management(generation, Ok(vec![workspace]));
    if let Mode::WorkspaceManager(manager) = &mut dashboard.mode {
        manager.sync_form();
        manager.form.get_mut().focus(WorkspaceControl::ConfirmClose);
    }
    assert_eq!(
        dashboard.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        DashboardAction::CloseWorkspace {
            generation,
            workspace_id: "a".into()
        }
    );
    assert!(
        matches!(&dashboard.mode, Mode::WorkspaceManager(manager) if manager.busy == Some(WorkspaceMutation::Close))
    );
    if let Mode::WorkspaceManager(manager) = &mut dashboard.mode {
        manager.sync_form();
        manager.form.get_mut().focus(WorkspaceControl::CancelClose);
    }
    assert_eq!(
        dashboard.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        DashboardAction::CancelWorkspaceClose {
            workspace_id: "a".into()
        }
    );
    assert_eq!(
        chord(&mut dashboard, crate::CommandId::QuitDetach),
        DashboardAction::QuitDetach
    );
    dashboard.finish_workspace_management(generation, Err("stop failed; retry".into()));
    assert!(
        matches!(&dashboard.mode, Mode::WorkspaceManager(manager) if manager.busy.is_none() && manager.error.as_deref() == Some("stop failed; retry"))
    );
}

#[test]
fn a_workspace_close_can_run_in_background_and_reopen_for_cancellation() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.set_active_workspace(Some("a".into()));
    let DashboardAction::LoadWorkspaceManagement { generation } =
        chord(&mut dashboard, crate::CommandId::CloseWorkspace)
    else {
        panic!("load");
    };
    dashboard.finish_workspace_management(generation, Ok(vec![entry("a", "Example")]));
    assert!(matches!(
        dashboard.workspace_manager_mutation(WorkspaceMutation::Close),
        DashboardAction::CloseWorkspace { .. }
    ));
    dashboard.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(
        matches!(dashboard.mode, Mode::Dashboard),
        "navigation is available while stopping"
    );
    let DashboardAction::LoadWorkspaceManagement {
        generation: reopened,
    } = chord(&mut dashboard, crate::CommandId::CloseWorkspace)
    else {
        panic!("reopen");
    };
    assert_ne!(generation, reopened);
    dashboard.finish_workspace_management(reopened, Ok(vec![entry("a", "Example")]));
    assert!(
        matches!(&dashboard.mode, Mode::WorkspaceManager(manager) if manager.busy == Some(WorkspaceMutation::Close))
    );
    let lines = draw_manager(&dashboard).join("\n");
    assert!(lines.contains("Cancel deletion"), "{lines}");
    assert!(lines.contains("Continue working"));
    assert_eq!(dashboard.workspace_close_finished("a"), Some(reopened));
    dashboard.finish_workspace_management(reopened, Err("stop failed".into()));
    assert!(
        matches!(&dashboard.mode, Mode::WorkspaceManager(manager) if manager.busy.is_none() && manager.error.as_deref() == Some("stop failed"))
    );
    assert!(
        matches!(
            dashboard.workspace_manager_mutation(WorkspaceMutation::Close),
            DashboardAction::CloseWorkspace { .. }
        ),
        "failure remains retryable"
    );
    assert_eq!(dashboard.workspace_close_finished("a"), Some(reopened));
    dashboard.finish_workspace_management(reopened, Ok(vec![]));
    // The dialog came from the Close shortcut, so a finished close returns
    // to the dashboard rather than to the manager's list (B-11).
    assert!(matches!(dashboard.mode, Mode::Dashboard));
}

/// Launch finding A-5: the Rename and Close shortcuts open their dialog
/// straight from the dashboard, so Esc there goes back to the dashboard
/// rather than to a Workspaces manager the user never opened.
#[test]
fn esc_in_a_workspace_dialog_opened_by_shortcut_returns_to_the_dashboard() {
    for command in [
        crate::CommandId::RenameWorkspace,
        crate::CommandId::CloseWorkspace,
    ] {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.set_active_workspace(Some("a".into()));
        let DashboardAction::LoadWorkspaceManagement { generation } =
            chord(&mut dashboard, command)
        else {
            panic!("load");
        };
        dashboard.finish_workspace_management(generation, Ok(vec![entry("a", "Example")]));
        assert!(
            matches!(&dashboard.mode, Mode::WorkspaceManager(manager) if manager.view != WorkspaceManagerView::List),
            "{command:?}"
        );

        dashboard.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(matches!(dashboard.mode, Mode::Dashboard), "{command:?}");
    }
}

/// Opened from the manager's list, the same dialog still goes back to the
/// list, which is where the user was.
#[test]
fn esc_in_a_workspace_dialog_opened_from_the_list_returns_to_the_list() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.set_active_workspace(Some("a".into()));
    let DashboardAction::LoadWorkspaceManagement { generation } =
        dashboard.begin_workspace_manager()
    else {
        panic!("load");
    };
    dashboard.finish_workspace_management(generation, Ok(vec![entry("a", "Example")]));
    if let Mode::WorkspaceManager(manager) = &mut dashboard.mode {
        manager.open_selected_command(false);
        manager.sync_form();
    }

    dashboard.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    assert!(
        matches!(&dashboard.mode, Mode::WorkspaceManager(manager) if manager.view == WorkspaceManagerView::List)
    );
}

/// Launch campaign finding D-13: after a restart the tabs read
/// `second  mjolnir` although they read `mjolnir  second` before, because a
/// fresh dashboard ordered tabs by random workspace id. The host orders them
/// by creation, and a later runtime update keeps that order.
#[test]
fn workspace_tabs_keep_the_hosts_order_across_updates() {
    let mut dashboard = dashboard_with_session(running_session());
    let names = std::collections::BTreeMap::from([
        ("f069".to_owned(), "mjolnir".to_owned()),
        ("2901".to_owned(), "second".to_owned()),
    ]);
    dashboard.set_workspace_names(names.clone());
    dashboard.order_workspaces(&["f069".to_owned(), "2901".to_owned()]);
    assert_eq!(dashboard.workspace_ids(), ["f069", "2901"]);
    dashboard.set_workspace_names(names);
    assert_eq!(dashboard.workspace_ids(), ["f069", "2901"]);
}

/// Launch finding B-11, Enter half: Enter in the Rename dialog opened by its
/// shortcut renamed the workspace but left the Workspaces manager open, a
/// screen the user never opened, and the dialog said "Esc closes manager".
/// Once the rename lands the dialog returns to the dashboard, as Esc does
/// since A-5.
#[test]
fn a_rename_opened_by_shortcut_returns_to_the_dashboard_when_it_lands() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.set_active_workspace(Some("a".into()));
    let DashboardAction::LoadWorkspaceManagement { generation } =
        chord(&mut dashboard, crate::CommandId::RenameWorkspace)
    else {
        panic!("load");
    };
    dashboard.finish_workspace_management(generation, Ok(vec![entry("a", "Example")]));
    let rendered = draw_manager(&dashboard).join("\n");
    assert!(!rendered.contains("Esc closes manager"), "{rendered}");
    assert!(
        rendered.contains("Enter saves the new name · Esc cancels"),
        "{rendered}"
    );
    if let Mode::WorkspaceManager(manager) = &mut dashboard.mode {
        manager.name = TextInput::from_value("Renamed");
    }
    assert!(matches!(
        dashboard.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        DashboardAction::RenameWorkspace { .. }
    ));

    assert!(dashboard.finish_workspace_management(generation, Ok(vec![entry("a", "Renamed")])));

    assert!(matches!(dashboard.mode, Mode::Dashboard));
}

/// The Close shortcut likewise returns to the dashboard once the close
/// finishes, rather than to the manager's list.
#[test]
fn a_close_opened_by_shortcut_returns_to_the_dashboard_when_it_finishes() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.set_active_workspace(Some("a".into()));
    let DashboardAction::LoadWorkspaceManagement { generation } =
        chord(&mut dashboard, crate::CommandId::CloseWorkspace)
    else {
        panic!("load");
    };
    dashboard.finish_workspace_management(
        generation,
        Ok(vec![entry("a", "Example"), entry("b", "Other")]),
    );
    if let Mode::WorkspaceManager(manager) = &mut dashboard.mode {
        manager.sync_form();
        manager.form.get_mut().focus(WorkspaceControl::ConfirmClose);
    }
    assert!(matches!(
        dashboard.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        DashboardAction::CloseWorkspace { .. }
    ));

    let generation = dashboard
        .workspace_close_finished("a")
        .unwrap_or(generation);
    dashboard.finish_workspace_management(generation, Ok(vec![entry("b", "Other")]));

    assert!(matches!(dashboard.mode, Mode::Dashboard));
}

/// Opened from the manager's list, a finished rename goes back to the list,
/// and the dialog says Esc does the same.
#[test]
fn a_rename_opened_from_the_list_returns_to_the_list() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.set_active_workspace(Some("a".into()));
    let DashboardAction::LoadWorkspaceManagement { generation } =
        dashboard.begin_workspace_manager()
    else {
        panic!("load");
    };
    dashboard.finish_workspace_management(generation, Ok(vec![entry("a", "Example")]));
    if let Mode::WorkspaceManager(manager) = &mut dashboard.mode {
        manager.open_selected_command(false);
        manager.sync_form();
    }
    let rendered = draw_manager(&dashboard).join("\n");
    assert!(
        rendered.contains("Enter saves the new name · Esc returns to the list"),
        "{rendered}"
    );
    if let Mode::WorkspaceManager(manager) = &mut dashboard.mode {
        manager.name = TextInput::from_value("Renamed");
    }
    assert!(matches!(
        dashboard.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        DashboardAction::RenameWorkspace { .. }
    ));

    dashboard.finish_workspace_management(generation, Ok(vec![entry("a", "Renamed")]));

    assert!(
        matches!(&dashboard.mode, Mode::WorkspaceManager(manager) if manager.view == WorkspaceManagerView::List)
    );
}
