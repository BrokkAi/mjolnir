use super::*;
use crate::test_support::{
    buffer_lines, chord, dashboard_with_session, drawn, key, mouse_at, point, prefix_key, route,
    running_session,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
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

fn append_workspace_state(output: &mut String, label: &str, lines: &[String]) {
    use std::fmt::Write as _;
    if !output.is_empty() {
        output.push('\n');
    }
    writeln!(output, "=== {label} (110x32) ===").unwrap();
    output.push_str(&lines.join("\n"));
    output.push('\n');
}

fn click_workspace_label(
    dashboard: &mut DashboardState,
    lines: &[String],
    label: &str,
) -> DashboardAction {
    let position = point(lines, label);
    let mut action = DashboardAction::None;
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        action = dashboard.handle_mouse(mouse_at(kind, position));
    }
    action
}

#[test]
fn golden_workspace_manager_drafts() {
    use std::fmt::Write as _;

    let mut output = String::new();
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.set_workspace_names(std::collections::BTreeMap::from([
        ("default".into(), "Default".into()),
        ("alpha".into(), "Alpha".into()),
        ("beta".into(), "Beta".into()),
    ]));
    dashboard.order_workspaces(&["default".into(), "alpha".into(), "beta".into()]);
    dashboard.set_active_workspace(Some("default".into()));

    let action = chord(&mut dashboard, crate::CommandId::FocusWorkspaces);
    writeln!(output, "action: {action:?}").unwrap();
    let action = dashboard.handle_key(key(KeyCode::Right));
    writeln!(output, "action: {action:?}").unwrap();
    dashboard.set_active_workspace(Some("alpha".into()));
    append_workspace_state(
        &mut output,
        "dashboard after selecting the next workspace",
        &drawn(&mut dashboard, 110, 32),
    );
    let action = route(
        &mut dashboard,
        &[
            prefix_key(),
            KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE),
        ],
    );
    writeln!(output, "action: {action:?}").unwrap();
    dashboard.set_active_workspace(Some("beta".into()));
    append_workspace_state(
        &mut output,
        "dashboard after selecting a numbered workspace",
        &drawn(&mut dashboard, 110, 32),
    );

    let action = chord(&mut dashboard, crate::CommandId::Workspaces);
    writeln!(output, "action: {action:?}").unwrap();
    let DashboardAction::LoadWorkspaceManagement { generation } = action else {
        panic!("workspace manager did not request its snapshot");
    };
    let mut beta = entry("beta", "Beta");
    beta.workspace.session_count = 2;
    beta.drafts = vec![
        WorkspaceDraftEntry {
            id: "draft-terminal".into(),
            session_id: Some("session-1".into()),
            source: "terminal composer".into(),
            saved_at: "2026-10-01 12:00".into(),
            owner_pid: Some(4100),
        },
        WorkspaceDraftEntry {
            id: "draft-web".into(),
            session_id: None,
            source: "web viewer".into(),
            saved_at: "2026-10-02 09:30".into(),
            owner_pid: None,
        },
    ];
    let mut alpha = entry("alpha", "Alpha");
    alpha.workspace.session_count = 1;
    dashboard.finish_workspace_management(
        generation,
        Ok(vec![
            entry("default", "Default"),
            alpha.clone(),
            beta.clone(),
        ]),
    );
    let lines = drawn(&mut dashboard, 110, 32);
    append_workspace_state(&mut output, "workspace manager list", &lines);
    let action = click_workspace_label(&mut dashboard, &lines, "Drafts");
    writeln!(output, "action: {action:?}").unwrap();
    append_workspace_state(
        &mut output,
        "drafts for the selected workspace",
        &drawn(&mut dashboard, 110, 32),
    );
    let action = dashboard.handle_key(key(KeyCode::Down));
    writeln!(output, "action: {action:?}").unwrap();
    append_workspace_state(
        &mut output,
        "second draft selected",
        &drawn(&mut dashboard, 110, 32),
    );
    let action = dashboard.handle_key(key(KeyCode::Enter));
    writeln!(output, "action: {action:?}").unwrap();

    beta.drafts.clear();
    dashboard.finish_workspace_management(
        generation,
        Ok(vec![entry("default", "Default"), alpha, beta]),
    );
    append_workspace_state(
        &mut output,
        "draft recovery completed",
        &drawn(&mut dashboard, 110, 32),
    );

    mj_core::golden::assert_golden(
        env!("CARGO_MANIFEST_DIR"),
        "workspace-manager-drafts",
        output.trim_end_matches('\n'),
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

    assert!(
        !dashboard
            .finish_workspace_management(generation, Ok(vec![entry("workspace-a", "Workspace A")]),)
            .foreground
    );
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

// Hard-won: 25c6af83: Close used placeholder (s) pluralization
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
    assert!(rendered.contains("Suspend 2 sessions and"), "{rendered}");
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
    let mut workspace = entry("a", "Example");
    workspace.workspace.session_count = 1;
    dashboard.finish_workspace_management(generation, Ok(vec![workspace]));
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
// Hard-won: 3f535f25: shortcut dialogs returned Esc to a manager the user never opened
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

/// Launch campaign finding D-13: after a restart the tabs read
/// `second  mjolnir` although they read `mjolnir  second` before, because a
/// fresh dashboard ordered tabs by random workspace id. The host orders them
/// by creation, and a later runtime update keeps that order.
// Hard-won: e923d3bf: tab order changed because workspace ids were random
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
// Hard-won: 11f841e4: shortcut Rename completion returned to an unopened manager
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

    assert!(
        dashboard
            .finish_workspace_management(generation, Ok(vec![entry("a", "Renamed")]))
            .foreground
    );

    assert!(matches!(dashboard.mode, Mode::Dashboard));
}

/// The Close shortcut likewise returns to the dashboard once the close
/// finishes, rather than to the manager's list.
// Hard-won: 11f841e4: shortcut Close completion returned to an unopened manager
#[test]
fn a_close_opened_by_shortcut_returns_to_the_dashboard_when_it_finishes() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.set_active_workspace(Some("a".into()));
    let DashboardAction::LoadWorkspaceManagement { generation } =
        chord(&mut dashboard, crate::CommandId::CloseWorkspace)
    else {
        panic!("load");
    };
    let mut workspace = entry("a", "Example");
    workspace.workspace.session_count = 1;
    dashboard.finish_workspace_management(generation, Ok(vec![workspace, entry("b", "Other")]));
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

fn draft() -> WorkspaceDraftEntry {
    WorkspaceDraftEntry {
        id: "draft-a".into(),
        session_id: None,
        source: "composer".into(),
        saved_at: "now".into(),
        owner_pid: None,
    }
}

#[test]
fn deleting_an_empty_workspace_from_its_shortcut_acts_without_confirmation() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.set_active_workspace(Some("a".into()));
    let DashboardAction::LoadWorkspaceManagement { generation } =
        chord(&mut dashboard, crate::CommandId::CloseWorkspace)
    else {
        panic!("load");
    };
    let outcome =
        dashboard.finish_workspace_management(generation, Ok(vec![entry("a", "Example")]));
    assert_eq!(
        outcome.action,
        DashboardAction::CloseWorkspace {
            generation,
            workspace_id: "a".into(),
        }
    );
    assert!(
        matches!(&dashboard.mode, Mode::WorkspaceManager(manager) if manager.busy == Some(WorkspaceMutation::Close)),
        "the delete is in flight at once"
    );
}

#[test]
fn deleting_a_workspace_with_sessions_or_drafts_from_its_shortcut_still_asks() {
    for (sessions, drafts) in [(2, false), (0, true)] {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.set_active_workspace(Some("a".into()));
        let DashboardAction::LoadWorkspaceManagement { generation } =
            chord(&mut dashboard, crate::CommandId::CloseWorkspace)
        else {
            panic!("load");
        };
        let mut workspace = entry("a", "Example");
        workspace.workspace.session_count = sessions;
        if drafts {
            workspace.drafts.push(draft());
        }
        let outcome = dashboard.finish_workspace_management(generation, Ok(vec![workspace]));
        assert_eq!(outcome.action, DashboardAction::None);
        assert!(matches!(&dashboard.mode, Mode::WorkspaceManager(manager)
                if manager.busy.is_none() && matches!(manager.view, WorkspaceManagerView::Close { .. })));
    }
}

#[test]
fn the_list_delete_button_acts_at_once_only_for_an_empty_workspace() {
    let mut dashboard = dashboard_with_session(running_session());
    let DashboardAction::LoadWorkspaceManagement { generation } =
        dashboard.begin_workspace_manager()
    else {
        panic!("load");
    };
    let mut busy = entry("busy", "Busy");
    busy.workspace.session_count = 1;
    dashboard.finish_workspace_management(generation, Ok(vec![busy, entry("empty", "Empty")]));
    let press_delete = |dashboard: &mut DashboardState, index: usize| {
        if let Mode::WorkspaceManager(manager) = &mut dashboard.mode {
            manager.select(index);
            manager.form.get_mut().focus(WorkspaceControl::Delete);
        }
        dashboard.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
    };
    assert_eq!(press_delete(&mut dashboard, 0), DashboardAction::None);
    assert!(matches!(
        &dashboard.mode,
        Mode::WorkspaceManager(manager) if matches!(manager.view, WorkspaceManagerView::Close { .. })
    ));
    if let Mode::WorkspaceManager(manager) = &mut dashboard.mode {
        manager.reset_to_list();
    }
    assert_eq!(
        press_delete(&mut dashboard, 1),
        DashboardAction::CloseWorkspace {
            generation,
            workspace_id: "empty".into(),
        }
    );
}
