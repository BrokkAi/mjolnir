use std::collections::BTreeMap;

use crossterm::event::{Event, KeyCode, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use mj_core::config::{ProjectBundle, ProjectRepository};
use mj_core::state::{STATE_VERSION, SessionState, State};

use super::*;
use crate::test_support::*;

use crate::render::render;

#[test]
fn rendering_and_inert_pointer_motion_leave_the_frame_clean() {
    let mut dashboard = dashboard_with_session(running_session());
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .unwrap();
    dashboard.take_render_changed();
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .unwrap();
    assert!(
        !dashboard.take_render_changed(),
        "geometry registration is not a visible mutation"
    );

    for column in 0..120 {
        let result = dashboard.handle_event_result(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column,
            row: 1,
            modifiers: KeyModifiers::NONE,
        }));
        assert_ne!(result.outcome, Outcome::Changed);
    }
    assert!(!dashboard.take_render_changed());
}

#[test]
fn event_result_distinguishes_selection_limit_from_movement() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.select_active_session("session-1");
    dashboard.take_render_changed();

    let result = dashboard.handle_event_result(Event::Key(key(KeyCode::Down)));

    assert_eq!(result.outcome, Outcome::Unchanged);
    assert!(result.action.is_none());
    assert!(!dashboard.take_render_changed());
}

#[test]
fn activating_target_rename_repaints_the_new_modal() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.begin_target_actions();
    dashboard.handle_event_result(Event::Key(key(KeyCode::Tab)));
    dashboard.take_render_changed();

    let result = dashboard.handle_event_result(Event::Key(key(KeyCode::Enter)));
    assert!(matches!(dashboard.mode, Mode::ConfigId(_)));
    assert_eq!(result.outcome, Outcome::Changed);
    assert!(dashboard.take_render_changed());
}

#[test]
fn event_result_repaints_for_a_cursor_only_text_edit() {
    let mut session = running_session();
    session.session_title_override = Some("rename me".into());
    let mut dashboard = dashboard_with_session(session);
    dashboard.select_active_session("session-1");
    dashboard.dispatch_command(CommandId::RenameSession);
    assert!(matches!(dashboard.mode, Mode::Rename(_)));
    dashboard.take_render_changed();

    let result = dashboard.handle_event_result(Event::Key(crossterm::event::KeyEvent::new(
        KeyCode::Left,
        KeyModifiers::NONE,
    )));

    assert_eq!(result.outcome, Outcome::Changed);
    assert!(result.action.is_none());
    assert!(dashboard.take_render_changed());
}

/// Opens the rename editor the way the surface offers it now: `F2`, type
/// enough of "rename" to pick it out, Enter. There is no `e` any more.
fn open_rename_through_the_palette(dashboard: &mut DashboardState) {
    dashboard.focus_sessions();
    dashboard.handle_key(key(KeyCode::F(2)));
    for character in "rename".chars() {
        dashboard.handle_key(key(KeyCode::Char(character)));
    }
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert!(
        matches!(dashboard.mode, Mode::Rename(_)),
        "{:?}",
        dashboard.mode
    );
}

/// The composer is a separate focus, so a pane's actions are plain
/// letters: nothing typed at a pane can be mistaken for prompt text.
#[test]
fn plain_keys_drive_the_focused_pane() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let mut dashboard = dashboard_with_session(session);

    assert_eq!(
        dashboard.handle_key(alt_key('s')),
        DashboardAction::OpenResumeDialog
    );
    dashboard.cancel_modal();
    assert_eq!(dashboard.handle_key(alt_key('w')), DashboardAction::None);
    assert!(matches!(dashboard.mode, Mode::New(_)));
    dashboard.cancel_modal();
    // `e` was the session edit dialog's key. The command palette replaced
    // that dialog, so nothing answers `e` on the Sessions pane now.
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Char('e'))),
        DashboardAction::None
    );
    assert_eq!(dashboard.mode, Mode::Dashboard);
    // Restart no longer answers a plain key: a session transition has to
    // be chosen from the palette or the row menu.
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Char('r'))),
        DashboardAction::None
    );
    assert_eq!(dashboard.mode, Mode::Dashboard);
    assert_eq!(
        dashboard.dispatch_command(CommandId::RestartSession),
        DashboardAction::RestartSession {
            session_id: "session-1".into()
        }
    );

    assert_eq!(
        dashboard.handle_key(key(KeyCode::F(3))),
        DashboardAction::None
    );
    assert_eq!(dashboard.mode, Mode::Dashboard);
    assert_eq!(
        dashboard.dispatch_command(CommandId::Workspaces),
        DashboardAction::LoadWorkspaceManagement { generation: 1 }
    );
    dashboard.cancel_modal();
    assert_eq!(
        dashboard.handle_key(key(KeyCode::F(4))),
        DashboardAction::LoadWebAccess
    );
    dashboard.cancel_modal();
    assert_eq!(
        dashboard.handle_key(ctrl_key('v')),
        DashboardAction::PasteFromClipboard
    );
}

#[test]
fn target_id_escape_restores_parent_action_focus() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.begin_target_actions();
    let Mode::TargetActions(dialog) = &mut dashboard.mode else {
        panic!("target actions should open");
    };
    dialog
        .form
        .get_mut()
        .focus(crate::dialogs::DialogControl::TargetRename);
    dashboard.handle_key(key(KeyCode::Enter));
    assert!(matches!(dashboard.mode, Mode::ConfigId(_)));
    dashboard.handle_key(key(KeyCode::Esc));
    let Mode::TargetActions(dialog) = &dashboard.mode else {
        panic!("Escape should restore target actions");
    };
    assert!(
        dialog
            .form
            .borrow()
            .is_focused(crate::dialogs::DialogControl::TargetRename)
    );
}

#[test]
fn pane_actions_follow_the_focused_pane() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let mut dashboard = dashboard_with_session(session);

    dashboard.handle_key(key(KeyCode::Tab));
    dashboard.handle_key(key(KeyCode::Tab));
    assert_eq!(dashboard.focus, Focus::Targets);
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert!(matches!(dashboard.mode, Mode::TargetActions(_)));
    dashboard.cancel_modal();
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Char('n'))),
        DashboardAction::None,
        "the plain letter creates nothing anywhere"
    );

    dashboard.handle_key(key(KeyCode::Tab));
    assert_eq!(dashboard.focus, Focus::Quota);
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Char('e'))),
        DashboardAction::None
    );
    assert!(matches!(dashboard.mode, Mode::ConfigId(_)));
}

#[test]
fn minimized_summary_panes_do_not_operate_on_hidden_rows() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);

    dashboard.focus = Focus::Targets;
    dashboard.set_pane_size(SupportPane::Targets, PaneSize::Minimized);
    dashboard.handle_key(key(KeyCode::Down));
    dashboard.handle_key(key(KeyCode::Enter));
    assert_eq!(dashboard.capacity_index, 0);
    assert_eq!(dashboard.mode, Mode::Dashboard);

    dashboard.focus = Focus::Quota;
    dashboard.set_pane_size(SupportPane::Quota, PaneSize::Minimized);
    dashboard.handle_key(key(KeyCode::Down));
    dashboard.handle_key(key(KeyCode::Char('e')));
    assert_eq!(dashboard.quota_index, 0);
    assert_eq!(dashboard.mode, Mode::Dashboard);
}

/// Refreshing moved off the two panes onto one global key, so the letter
/// the panes used to answer must now do nothing at all.
#[test]
fn plain_r_no_longer_refreshes() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let mut dashboard = dashboard_with_session(session);

    for wanted in [Focus::Targets, Focus::Quota] {
        while dashboard.focus != wanted {
            dashboard.cycle_focus(false);
        }
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Char('r'))),
            DashboardAction::None,
            "plain r still acts at {wanted:?}"
        );
        assert_eq!(dashboard.mode, Mode::Dashboard);
    }

    // F5 is the one refresh key, and it answers from every pane.
    assert_eq!(
        dashboard.handle_key(key(KeyCode::F(5))),
        DashboardAction::RefreshAll
    );
}

#[test]
fn tab_walks_the_layout_order_and_back() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let mut dashboard = dashboard_with_session(session);
    assert_eq!(dashboard.focus, Focus::Sessions);

    // The ring follows the layout down the screen.
    for expected in [
        Focus::Prompt,
        Focus::Targets,
        Focus::Quota,
        Focus::Workspaces,
        Focus::Workspaces,
        Focus::Sessions,
    ] {
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Tab)),
            DashboardAction::None
        );
        assert_eq!(dashboard.focus, expected);
    }
}

#[test]
fn shift_tab_walks_the_reverse_order() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let mut dashboard = dashboard_with_session(session);

    for expected in [
        Focus::Workspaces,
        Focus::Workspaces,
        Focus::Quota,
        Focus::Targets,
        Focus::Prompt,
        Focus::Sessions,
    ] {
        dashboard.handle_key(key(KeyCode::BackTab));
        assert_eq!(dashboard.focus, expected);
    }
}

#[test]
fn alt_g_toggles_standard_and_minimized_panes_without_moving_focus() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let mut dashboard = dashboard_with_session(session);
    dashboard.focus = Focus::Quota;
    assert_eq!(
        dashboard.pane_size(SupportPane::Sessions),
        PaneSize::Standard
    );

    assert_eq!(dashboard.handle_key(alt_key('g')), DashboardAction::None);
    assert_eq!(
        dashboard.pane_size(SupportPane::Sessions),
        PaneSize::Minimized
    );
    assert_eq!(
        dashboard.pane_size(SupportPane::Targets),
        PaneSize::Minimized
    );
    assert_eq!(dashboard.pane_size(SupportPane::Quota), PaneSize::Minimized);
    assert_eq!(dashboard.focus, Focus::Quota);

    assert_eq!(dashboard.handle_key(alt_key('g')), DashboardAction::None);
    for pane in [
        SupportPane::Sessions,
        SupportPane::Targets,
        SupportPane::Quota,
    ] {
        assert_eq!(dashboard.pane_size(pane), PaneSize::Standard);
    }
    assert_eq!(dashboard.focus, Focus::Quota);
}

#[test]
fn alt_z_cycles_the_focused_pane_and_a_new_maximum_demotes_the_old_one() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.focus = Focus::Sessions;
    dashboard.handle_key(alt_key('z'));
    assert_eq!(
        dashboard.pane_size(SupportPane::Sessions),
        PaneSize::Maximized
    );
    assert_eq!(dashboard.focus, Focus::Sessions);

    dashboard.focus = Focus::Targets;
    dashboard.handle_key(alt_key('z'));
    assert_eq!(
        dashboard.pane_size(SupportPane::Targets),
        PaneSize::Maximized
    );
    assert_eq!(
        dashboard.pane_size(SupportPane::Sessions),
        PaneSize::Standard
    );
    assert_eq!(dashboard.pane_size(SupportPane::Quota), PaneSize::Standard);

    dashboard.handle_key(alt_key('z'));
    assert_eq!(
        dashboard.pane_size(SupportPane::Targets),
        PaneSize::Minimized
    );
    dashboard.handle_key(alt_key('z'));
    assert_eq!(
        dashboard.pane_size(SupportPane::Targets),
        PaneSize::Standard
    );
}

#[test]
fn alt_z_skips_a_maximum_that_cannot_grow_the_focused_pane() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.focus = Focus::Targets;
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw dashboard");

    assert!(!dashboard.pane_maximize_enabled(SupportPane::Targets));
    dashboard.handle_key(alt_key('z'));
    assert_eq!(
        dashboard.pane_size(SupportPane::Targets),
        PaneSize::Minimized
    );
    assert_eq!(dashboard.focus, Focus::Targets);
}

#[test]
fn pane_sizes_capture_and_restore_a_nondefault_arrangement() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.set_pane_size(SupportPane::Sessions, PaneSize::Maximized);
    dashboard.set_pane_size(SupportPane::Targets, PaneSize::Minimized);
    let captured = dashboard.pane_sizes();

    dashboard.set_pane_size(SupportPane::Quota, PaneSize::Maximized);
    dashboard.set_pane_maximize_enabled([
        (SupportPane::Sessions, false),
        (SupportPane::Targets, true),
        (SupportPane::Quota, true),
    ]);
    dashboard.restore_pane_sizes(captured).unwrap();

    assert_eq!(dashboard.pane_sizes(), captured);
    assert_eq!(
        dashboard.pane_size(SupportPane::Sessions),
        PaneSize::Maximized
    );
    assert_eq!(
        dashboard.pane_size(SupportPane::Targets),
        PaneSize::Minimized
    );
    assert_eq!(dashboard.pane_size(SupportPane::Quota), PaneSize::Standard);
}

#[test]
fn invalid_pane_size_restore_leaves_the_current_arrangement_unchanged() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.set_pane_size(SupportPane::Targets, PaneSize::Minimized);
    let before = dashboard.pane_sizes();
    let invalid = PaneSizes {
        sessions: PaneSize::Maximized,
        targets: PaneSize::Maximized,
        quota: PaneSize::Standard,
    };

    assert!(dashboard.restore_pane_sizes(invalid).is_err());
    assert_eq!(dashboard.pane_sizes(), before);
}

#[test]
fn alt_z_on_prompt_explains_that_prompt_is_not_resizable() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.focus_prompt();
    dashboard.handle_key(alt_key('z'));
    assert_eq!(
        dashboard.notice().as_deref(),
        Some("Select Sessions, Targets, or Quota before pressing Alt-Z.")
    );
    assert_eq!(dashboard.focus, Focus::Prompt);
}

/// Plain letters are pane-local only. New session, resume, and mark read
/// keep pane-local shortcuts separate from global chords.
#[test]
fn plain_a_remains_unbound_and_the_wizard_has_its_own_key() {
    {
        let character = 'a';
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.focus_sessions();

        assert_eq!(
            dashboard.handle_key(key(KeyCode::Char(character))),
            DashboardAction::None,
            "{character}"
        );
        assert_eq!(dashboard.mode, Mode::Dashboard, "{character}");
        assert_eq!(dashboard.notice(), None, "{character}");
    }

    // The chords still do what the letters used to.
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.focus_sessions();
    assert_eq!(dashboard.handle_key(alt_key('w')), DashboardAction::None);
    assert!(matches!(dashboard.mode, Mode::New(_)));
    dashboard.cancel_modal();

    assert_eq!(
        dashboard.handle_key(alt_key('s')),
        DashboardAction::OpenResumeDialog
    );
    assert_eq!(dashboard.handle_key(alt_key('a')), DashboardAction::None);
    assert_eq!(dashboard.notice().as_deref(), Some("No unread sessions."));
}

#[test]
fn ctrl_c_is_inert_on_an_empty_dashboard_and_every_pane() {
    for focus in [
        Focus::Sessions,
        Focus::Prompt,
        Focus::Targets,
        Focus::Quota,
        Focus::Workspaces,
    ] {
        let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
        dashboard.focus = focus;

        for _ in 0..2 {
            assert_eq!(
                dashboard.handle_key(ctrl_key('c')),
                DashboardAction::None,
                "Ctrl-C should not quit from {focus:?}"
            );
            assert_eq!(dashboard.mode, Mode::Dashboard, "{focus:?}");
            assert_eq!(dashboard.focus, focus, "{focus:?}");
        }
    }
}

#[test]
fn ctrl_c_cancels_text_modal_but_is_inert_on_non_text_modal_controls() {
    let mut rename = dashboard_with_session(running_session());
    open_rename_through_the_palette(&mut rename);
    assert_eq!(rename.handle_key(ctrl_key('c')), DashboardAction::None);
    assert_eq!(rename.mode, Mode::Dashboard);
    // A second press remains harmless after the text modal has closed.
    assert_eq!(rename.handle_key(ctrl_key('c')), DashboardAction::None);
    assert_eq!(rename.mode, Mode::Dashboard);

    let mut new_session = dashboard_with_session(running_session());
    assert_eq!(new_session.handle_key(alt_key('w')), DashboardAction::None);
    let mode_before_ctrl_c = new_session.mode.clone();
    assert!(matches!(mode_before_ctrl_c, Mode::New(_)));
    assert_eq!(new_session.handle_key(ctrl_key('c')), DashboardAction::None);
    assert_eq!(new_session.mode, mode_before_ctrl_c);
    // The wizard is still open, and repeated Ctrl-C does not activate a
    // control whose label happens to contain the letter `c`.
    assert_eq!(new_session.handle_key(ctrl_key('c')), DashboardAction::None);
    assert!(matches!(new_session.mode, Mode::New(_)));
}

#[test]
fn tab_reaches_every_pane_without_changing_explicit_sizes() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let mut dashboard = dashboard_with_session(session);
    dashboard.set_pane_size(SupportPane::Targets, PaneSize::Minimized);
    dashboard.set_pane_size(SupportPane::Quota, PaneSize::Minimized);
    let sizes = dashboard.pane_sizes;
    for expected in [
        Focus::Prompt,
        Focus::Targets,
        Focus::Quota,
        Focus::Workspaces,
        Focus::Workspaces,
        Focus::Sessions,
    ] {
        dashboard.handle_key(key(KeyCode::Tab));
        assert_eq!(dashboard.focus, expected);
        assert_eq!(dashboard.pane_sizes, sizes);
    }
    for expected in [
        Focus::Workspaces,
        Focus::Workspaces,
        Focus::Quota,
        Focus::Targets,
        Focus::Prompt,
        Focus::Sessions,
    ] {
        dashboard.handle_key(key(KeyCode::BackTab));
        assert_eq!(dashboard.focus, expected);
        assert_eq!(dashboard.pane_sizes, sizes);
    }
}

/// The combined surface is quit with Alt-Q. A stray Escape must never
/// take the conversation off the screen.
#[test]
fn escape_never_quits_the_combined_surface() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let mut dashboard = dashboard_with_session(session);

    for expected in [
        Focus::Sessions,
        Focus::Prompt,
        Focus::Targets,
        Focus::Quota,
        Focus::Workspaces,
    ] {
        assert_eq!(dashboard.focus, expected);
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Esc)),
            DashboardAction::None,
            "{expected:?}"
        );
        dashboard.handle_key(key(KeyCode::Tab));
    }
}

#[test]
fn ctrl_n_and_ctrl_p_move_the_focused_list() {
    let sessions = (0..3)
        .map(|index| {
            let mut session = stopped_session();
            session.id = format!("session-{index}");
            session.state = SessionState::Running;
            (session.id.clone(), session)
        })
        .collect();
    let mut dashboard = DashboardState::new(
        config(),
        State {
            subagents: Default::default(),
            version: STATE_VERSION,
            sessions,
            mount_history: BTreeMap::new(),
            container_sizes: BTreeMap::new(),
        },
        BTreeMap::new(),
    );

    assert_eq!(dashboard.selected_visible_index(), Some(0));
    dashboard.handle_key(ctrl_key('n'));
    dashboard.handle_key(ctrl_key('n'));
    assert_eq!(dashboard.selected_visible_index(), Some(2));
    dashboard.handle_key(ctrl_key('p'));
    assert_eq!(dashboard.selected_visible_index(), Some(1));

    dashboard.handle_key(key(KeyCode::BackTab));
    assert_eq!(dashboard.focus, Focus::Workspaces);
    dashboard.handle_key(key(KeyCode::BackTab));
    assert_eq!(dashboard.focus, Focus::Workspaces);
    dashboard.handle_key(key(KeyCode::BackTab));
    assert_eq!(dashboard.focus, Focus::Quota);
    dashboard.handle_key(ctrl_key('n'));
    assert_eq!(dashboard.quota_index, 1);
    dashboard.handle_key(ctrl_key('p'));
    assert_eq!(dashboard.quota_index, 0);
}

/// Builds `count` live sessions, `per_project` of them in each project,
/// so the compact list's threshold and grouping can be exercised.
fn dashboard_with_live_sessions(count: usize, per_project: usize) -> DashboardState {
    let sessions = (0..count)
        .map(|index| {
            let mut session = stopped_session();
            session.id = format!("session-{index}");
            session.state = SessionState::Running;
            session.created_at = format!("2026-08-{:02}T00:00:00Z", index + 1);
            session.project_directory =
                Some(format!("/projects/p{}", index / per_project.max(1)).into());
            (session.id.clone(), session)
        })
        .collect();
    DashboardState::new(
        config(),
        State {
            subagents: Default::default(),
            version: STATE_VERSION,
            sessions,
            mount_history: BTreeMap::new(),
            container_sizes: BTreeMap::new(),
        },
        BTreeMap::new(),
    )
}

fn session_row_indices(dashboard: &DashboardState) -> Vec<usize> {
    dashboard
        .sessions_rows()
        .into_iter()
        .filter_map(|row| match row {
            SessionsRow::Session { index, .. } => Some(index),
            _ => None,
        })
        .collect()
}

/// Every explicit size lists every session across every project.
#[test]
fn every_pane_size_lists_every_session_across_projects() {
    for size in [PaneSize::Minimized, PaneSize::Standard, PaneSize::Maximized] {
        let mut dashboard = dashboard_with_live_sessions(6, 2);
        dashboard.set_pane_size(SupportPane::Sessions, size);
        dashboard.set_current_session(Some("session-0"));

        assert_eq!(
            session_row_indices(&dashboard),
            [0, 1, 2, 3, 4, 5],
            "{size:?}"
        );
        assert_eq!(
            dashboard.visible_session_indices(),
            [0, 1, 2, 3, 4, 5],
            "{size:?}"
        );
        // Three projects, each with a heading.
        let headings = dashboard
            .sessions_rows()
            .into_iter()
            .filter(|row| matches!(row, SessionsRow::ProjectHeading { .. }))
            .count();
        assert_eq!(headings, 3, "{size:?}");
    }
}

#[test]
fn every_project_starts_expanded_and_collapsing_one_leaves_the_others() {
    let mut dashboard = dashboard_with_live_sessions(4, 2);
    dashboard.focus_sessions();
    let expanded = |dashboard: &DashboardState| {
        dashboard
            .sessions_rows()
            .into_iter()
            .filter_map(|row| match row {
                SessionsRow::Session { expanded, .. } => Some(expanded),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(expanded(&dashboard), [true, true, true, true]);

    dashboard.handle_key(key(KeyCode::Char('2')));
    assert_eq!(expanded(&dashboard), [true, true, false, false]);
    dashboard.handle_key(key(KeyCode::Char(' ')));
    assert_eq!(
        expanded(&dashboard),
        [false, false, false, false],
        "Space collapses the selected session's own project"
    );
}

/// A session that becomes history must be removed from the live list and
/// the selection must land on a real remaining row.
#[test]
fn the_selection_survives_the_list_changing_under_it() {
    let mut dashboard = dashboard_with_live_sessions(3, 3);
    dashboard.focus_sessions();
    dashboard.select_active_session("session-1");
    assert_eq!(dashboard.selected_session().unwrap().id, "session-1");

    // Focus moving away and back does not move the selection.
    dashboard.handle_key(key(KeyCode::Tab));
    dashboard.handle_key(key(KeyCode::BackTab));
    assert_eq!(dashboard.selected_session().unwrap().id, "session-1");

    let mut state = dashboard.state.clone();
    state.sessions.get_mut("session-1").unwrap().state = SessionState::Stopped;
    dashboard.set_state(state);
    assert_eq!(
        dashboard.selected_session().unwrap().id,
        "session-0",
        "history is excluded and selection clamps to the first live row"
    );
}

/// Each numbered project answers only for itself, so several can be
/// collapsed at once and the rest stay expanded.
#[test]
fn digits_toggle_projects_independently() {
    let sessions = (0..3)
        .map(|index| {
            let mut session = stopped_session();
            session.id = format!("session-{index}");
            session.state = SessionState::Running;
            session.project_directory = Some(format!("/projects/p{index}").into());
            (session.id.clone(), session)
        })
        .collect();
    let mut dashboard = DashboardState::new(
        config(),
        State {
            subagents: Default::default(),
            version: STATE_VERSION,
            sessions,
            mount_history: BTreeMap::new(),
            container_sizes: BTreeMap::new(),
        },
        BTreeMap::new(),
    );
    let keys = dashboard.project_keys();
    assert_eq!(keys.len(), 3);
    let expanded = |dashboard: &DashboardState| {
        dashboard
            .project_keys()
            .into_iter()
            .map(|key| !dashboard.collapsed_project_keys.contains(&key))
            .collect::<Vec<_>>()
    };
    assert_eq!(expanded(&dashboard), [true, true, true]);

    dashboard.handle_key(key(KeyCode::Char('1')));
    assert_eq!(expanded(&dashboard), [false, true, true]);
    dashboard.handle_key(key(KeyCode::Char('3')));
    assert_eq!(expanded(&dashboard), [false, true, false]);
    dashboard.handle_key(key(KeyCode::Char('1')));
    assert_eq!(expanded(&dashboard), [true, true, false]);
}

#[test]
fn remote_operation_cancel_action_carries_the_operation_kind() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let session_id = session.id.clone();
    let mut dashboard = dashboard_with_session(session);
    dashboard.begin_session_operation(session_id.clone(), SessionOperationKind::Launching, None);

    assert_eq!(
        dashboard.handle_key(alt_key('x')),
        DashboardAction::CancelOperation {
            session_id,
            kind: SessionOperationKind::Launching,
        }
    );
}

/// A launching session parks its conversation behind a composer the user
/// can type into; the draft survives to be taken by the chat that opens.
#[test]
fn typing_during_a_launching_transition_edits_the_standby_draft() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let mut dashboard = dashboard_with_session(session);
    dashboard.begin_session_operation("session-1".into(), SessionOperationKind::Launching, None);
    dashboard.focus_prompt();

    for character in "hello".chars() {
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Char(character))),
            DashboardAction::None
        );
    }
    dashboard.handle_key(key(KeyCode::Backspace));
    dashboard.handle_key(key(KeyCode::Char('l')));

    assert_eq!(
        dashboard
            .standby_prompts
            .get("session-1")
            .map(|standby| standby.draft()),
        Some("helll".into())
    );
    assert_eq!(
        dashboard.take_standby_prompt_draft("session-1").as_deref(),
        Some("helll")
    );
    assert_eq!(dashboard.take_standby_prompt_draft("session-1"), None);
}

/// The standby composer is the real one, so its readline chords edit the
/// draft instead of falling through to the dashboard.
#[test]
fn readline_chords_edit_the_standby_draft() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let mut dashboard = dashboard_with_session(session);
    dashboard.begin_session_operation("session-1".into(), SessionOperationKind::Resuming, None);
    dashboard.focus_prompt();

    for character in "alpha beta".chars() {
        dashboard.handle_key(key(KeyCode::Char(character)));
    }
    // Ctrl-A to line start, then Ctrl-K kills the whole line…
    dashboard.handle_key(ctrl_key('a'));
    dashboard.handle_key(ctrl_key('k'));
    assert_eq!(
        dashboard
            .standby_prompts
            .get("session-1")
            .map(|standby| standby.draft()),
        Some(String::new())
    );
    // …Ctrl-Y yanks it back, and Alt-B walks back a word.
    dashboard.handle_key(ctrl_key('y'));
    dashboard.handle_key(alt_key('b'));
    assert_eq!(
        dashboard
            .standby_prompts
            .get("session-1")
            .map(|standby| standby.draft()),
        Some("alpha beta".into())
    );
    assert!(dashboard.take_standby_prompt_draft("session-1").is_some());
}

/// Enter must not send while the session is not live: it is consumed with
/// an explanation and the draft stays editable.
#[test]
fn enter_during_a_starting_transition_does_not_send_or_clear_the_draft() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let mut dashboard = dashboard_with_session(session);
    dashboard.begin_session_operation("session-1".into(), SessionOperationKind::Launching, None);
    dashboard.focus_prompt();
    dashboard.handle_key(key(KeyCode::Char('h')));

    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert!(dashboard.notice().is_some());
    assert_eq!(
        dashboard
            .standby_prompts
            .get("session-1")
            .map(|standby| standby.draft()),
        Some("h".into())
    );
}

/// Retiring transitions have no conversation to type toward, so their
/// keys keep falling through to the ordinary dashboard handling.
#[test]
fn a_stopping_transition_offers_no_standby_composer() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let mut dashboard = dashboard_with_session(session);
    dashboard.begin_session_operation("session-1".into(), SessionOperationKind::Stopping, None);
    dashboard.focus_prompt();

    assert_eq!(
        dashboard.handle_key(key(KeyCode::Char('h'))),
        DashboardAction::None
    );
    assert!(!dashboard.standby_prompts.contains_key("session-1"));
}

/// A paste into the parked composer lands in the draft with terminal line
/// endings normalized, the way the chat composer normalizes them.
#[test]
fn a_paste_during_a_starting_transition_joins_the_standby_draft() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let mut dashboard = dashboard_with_session(session);
    dashboard.begin_session_operation("session-1".into(), SessionOperationKind::Resuming, None);
    dashboard.focus_prompt();

    dashboard.handle_paste("first\r\nsecond\rthird");

    assert_eq!(
        dashboard
            .standby_prompts
            .get("session-1")
            .map(|standby| standby.draft()),
        Some("first\nsecond\nthird".into())
    );
}

#[test]
fn opening_a_session_with_missing_configuration_explains_repair() {
    let session = running_session();
    let mut dashboard = dashboard_with_session(session);
    let bundle_id = dashboard.selected_session().unwrap().bundle_id.clone();
    let bundle = dashboard.config.bundles.remove(&bundle_id).unwrap();
    assert_eq!(dashboard.open_selected_session(), DashboardAction::None);
    let Mode::Confirm(dialog) = &dashboard.mode else {
        panic!("repair dialog expected")
    };
    let Confirmation::ConfigurationRepair { error, .. } = &dialog.confirmation else {
        panic!("repair details expected")
    };
    assert!(error.contains("missing bundle"));
    assert!(error.contains("config.toml"));
    dashboard.handle_key(key(KeyCode::Esc));
    dashboard.config.bundles.insert(bundle_id, bundle);
    assert!(!matches!(
        dashboard.open_selected_session(),
        DashboardAction::None
    ));
}

/// The notice bar is the only report a background failure gets, so a key
/// press that happens to arrive while one is fresh must not wipe it.
#[test]
fn a_fresh_notice_survives_a_key_press_and_clears_once_it_has_been_readable() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let mut dashboard = dashboard_with_session(session);

    dashboard.set_notice("Rename failed: relay unreachable");
    let shown_at = Instant::now();

    assert_eq!(
        dashboard.handle_key_at(key(KeyCode::Down), shown_at),
        DashboardAction::None
    );
    assert_eq!(
        dashboard.notice().as_deref(),
        Some("Rename failed: relay unreachable")
    );

    assert_eq!(
        dashboard.handle_key_at(
            key(KeyCode::Down),
            shown_at + mj_chat::chat::NOTICE_MINIMUM_DISPLAY
        ),
        DashboardAction::None
    );
    assert_eq!(dashboard.notice(), None);
}

/// A key press that reports something of its own replaces the notice
/// whatever its age; the display period only defends against incidental
/// keys.
#[test]
fn a_key_press_with_its_own_notice_replaces_a_fresh_one() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let mut dashboard = dashboard_with_session(session);

    dashboard.set_notice("Rename failed: relay unreachable");
    let shown_at = Instant::now();

    assert_eq!(
        dashboard.handle_key_at(alt_key('a'), shown_at),
        DashboardAction::None
    );
    assert_eq!(dashboard.notice().as_deref(), Some("No unread sessions."));
}

#[test]
fn alt_q_quits_without_mutating_any_dashboard_modal() {
    let mut new_session = DashboardState::new(config(), State::default(), BTreeMap::new());
    assert_eq!(new_session.handle_key(alt_key('w')), DashboardAction::None);

    let mut resume = dashboard_with_session(stopped_session());
    assert_eq!(open_resume_wizard(&mut resume), DashboardAction::None);

    let mut running = stopped_session();
    running.state = SessionState::Running;
    running.checkpoint = None;
    let mut rename = dashboard_with_session(running);
    open_rename_through_the_palette(&mut rename);

    let mut importing = dashboard_with_session(stopped_session());
    importing.show_import_progress("Chosen session".into());

    let mut confirm_import = dashboard_with_session(stopped_session());
    confirm_import.show_import_bundle_confirmation(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        false,
        Default::default(),
    );

    let mut confirm = dashboard_with_session(stopped_session());
    confirm.mode = Mode::Confirm(dialogs::ConfirmDialog::new(
        dialogs::Confirmation::ForceDestroy {
            session_id: "session-1".into(),
        },
    ));

    let mut resume_dialog = dashboard_with_session(stopped_session());
    resume_dialog.show_resume_dialog(1, Vec::new());

    for (label, mut dashboard) in [
        ("new session", new_session),
        ("resume", resume),
        ("resume dialog", resume_dialog),
        ("rename", rename),
        ("import progress", importing),
        ("import confirmation", confirm_import),
        ("confirmation", confirm),
    ] {
        assert!(!matches!(dashboard.mode, Mode::Dashboard), "{label}");
        let mode_before_quit = dashboard.mode.clone();

        // Alt-Q is a global chord: the controller answers it before the
        // surface sees the key, so this drives the same path the
        // controller's pre-filter drives.
        let command = crate::global_chord(&alt_key('q')).expect("Alt-Q is a global chord");
        assert!(dashboard.global_chord_allowed(command), "{label}");
        assert_eq!(
            dashboard.dispatch_command(command),
            DashboardAction::QuitDetach,
            "{label}"
        );
        assert_eq!(dashboard.mode, mode_before_quit, "{label}");
    }
}

#[test]
fn session_order_cache_tracks_creation_visibility_and_workspace_changes() {
    let mut first = running_session();
    first.id = "first".into();
    first.created_at = "2026-01-01T00:00:00Z".into();
    let mut second = first.clone();
    second.id = "second".into();
    second.created_at = "2026-01-02T00:00:00Z".into();
    let mut dashboard = dashboard_with_session(first);
    dashboard.state.sessions.insert(second.id.clone(), second);
    let ids = |dashboard: &DashboardState| {
        dashboard
            .ordered_sessions()
            .iter()
            .map(|session| session.id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&dashboard), ["first", "second"]);
    assert_eq!(ids(&dashboard), ["first", "second"]);
    dashboard
        .state
        .sessions
        .get_mut("second")
        .unwrap()
        .created_at = "2025-01-01T00:00:00Z".into();
    assert_eq!(ids(&dashboard), ["second", "first"]);
    dashboard.state.sessions.get_mut("second").unwrap().state = SessionState::Stopped;
    assert_eq!(ids(&dashboard), ["first"]);
    dashboard.config.advanced.show_stopped_sessions = true;
    assert_eq!(ids(&dashboard), ["second", "first"]);
    dashboard
        .state
        .sessions
        .get_mut("second")
        .unwrap()
        .workspace_id = "elsewhere".into();
    assert_eq!(ids(&dashboard), ["first"]);
}

#[test]
fn workspace_arrows_keep_focus_while_restoring_other_workspace_views() {
    let mut dashboard = dashboard_with_session(running_session());
    let first = dashboard.active_workspace_id().unwrap().to_owned();
    dashboard.workspace_order.push("second".into());
    dashboard.workspace_order.push("third".into());
    dashboard
        .workspace_names
        .insert("second".into(), "Second".into());
    dashboard
        .workspace_names
        .insert("third".into(), "Third".into());
    dashboard.set_active_workspace(Some("second".into()));
    dashboard.focus_prompt();
    dashboard.set_active_workspace(Some(first.clone()));
    dashboard.handle_key(key(KeyCode::BackTab));
    assert_eq!(dashboard.focus, Focus::Workspaces);
    dashboard.handle_key(key(KeyCode::BackTab));
    assert_eq!(dashboard.focus, Focus::Workspaces);
    for expected in ["second", "third"] {
        let DashboardAction::SelectWorkspace { workspace_id } =
            dashboard.handle_key(key(KeyCode::Right))
        else {
            panic!("right selects the next workspace");
        };
        assert_eq!(workspace_id, expected);
        dashboard.set_active_workspace(Some(workspace_id));
        assert_eq!(dashboard.focus, Focus::Workspaces);
    }
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Right)),
        DashboardAction::None
    );
    assert_eq!(
        dashboard.workspace_control_focus,
        crate::workspaces::WorkspaceControlFocus::Menu
    );
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Left)),
        DashboardAction::None
    );
    let DashboardAction::SelectWorkspace { workspace_id } =
        dashboard.handle_key(key(KeyCode::Left))
    else {
        panic!("left selects the previous workspace");
    };
    assert_eq!(workspace_id, "second");
    dashboard.set_active_workspace(Some(workspace_id));
    assert_eq!(dashboard.focus, Focus::Workspaces);
    dashboard.handle_key(key(KeyCode::Tab));
    assert_eq!(
        dashboard.workspace_control_focus,
        crate::workspaces::WorkspaceControlFocus::Menu
    );
    dashboard.handle_key(key(KeyCode::Tab));
    assert_eq!(dashboard.focus, Focus::Sessions);
}

#[test]
fn workspace_tabs_filter_live_sessions_and_keep_transitions_visible() {
    let mut local = running_session();
    local.id = "local".into();
    let mut dashboard = dashboard_with_session(local);
    let mut remote = running_session();
    remote.id = "remote".into();
    remote.workspace_id = "another-workspace".into();
    dashboard.state.sessions.insert(remote.id.clone(), remote);
    let mut archived = running_session();
    archived.id = "archived".into();
    archived.archived = true;
    dashboard
        .state
        .sessions
        .insert(archived.id.clone(), archived);
    let mut history = stopped_session();
    history.id = "history".into();
    dashboard.state.sessions.insert(history.id.clone(), history);
    dashboard.clamp_selections();
    assert_eq!(
        dashboard
            .ordered_sessions()
            .iter()
            .map(|session| session.id.as_str())
            .collect::<Vec<_>>(),
        ["archived", "local"]
    );
    dashboard.set_active_workspace(Some("another-workspace".into()));
    assert_eq!(
        dashboard
            .ordered_sessions()
            .iter()
            .map(|session| session.id.as_str())
            .collect::<Vec<_>>(),
        ["remote"]
    );
    dashboard.session_operations.insert(
        "history".into(),
        operation(SessionOperationKind::Launching, None),
    );
    dashboard.set_active_workspace(Some(mj_core::workspace::DEFAULT_WORKSPACE_ID.into()));
    assert!(
        dashboard
            .ordered_sessions()
            .iter()
            .any(|session| session.id == "local")
    );
    assert!(
        dashboard
            .ordered_sessions()
            .iter()
            .any(|session| session.id == "history")
    );
}

#[test]
fn subagent_workspace_filters_children_and_closes_back_to_named_parent() {
    let mut parent = stopped_session();
    parent.id = "parent-session".into();
    parent.title = "Parent planning session".into();
    parent.session_title_override = Some("Parent planning session".into());
    parent.state = SessionState::Running;
    let mut child = stopped_session();
    child.id = "child-session".into();
    child.title = "Inspect parser".into();
    child.state = SessionState::Running;
    let relation = mj_core::subagent::SubagentRecord {
        child_session_id: child.id.clone(),
        parent_session_id: parent.id.clone(),
        task_name: "Inspect parser".into(),
        profile_id: child.last_profile.clone(),
        model: None,
        effort: None,
        working_directory: Default::default(),
        initial_prompt: "Inspect the parser".into(),
        request_key: "request-1".into(),
        created_at: child.created_at.clone(),
        noticed_turn: None,
    };
    let mut dashboard = DashboardState::new(
        config(),
        State {
            subagents: BTreeMap::from([(child.id.clone(), relation)]),
            version: STATE_VERSION,
            sessions: BTreeMap::from([
                (parent.id.clone(), parent.clone()),
                (child.id.clone(), child.clone()),
            ]),
            mount_history: BTreeMap::new(),
            container_sizes: BTreeMap::new(),
        },
        BTreeMap::new(),
    );

    assert_eq!(
        dashboard
            .ordered_sessions()
            .iter()
            .map(|session| session.id.as_str())
            .collect::<Vec<_>>(),
        vec![parent.id.as_str()],
        "child sessions belong only to the virtual workspace"
    );

    dashboard.open_subagent_workspace(parent.id.clone());
    assert_eq!(dashboard.subagent_parent_id(), Some(parent.id.as_str()));
    assert_eq!(
        dashboard
            .ordered_sessions()
            .iter()
            .map(|session| session.id.as_str())
            .collect::<Vec<_>>(),
        vec![child.id.as_str()]
    );

    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 3)).unwrap();
    terminal
        .draw(|frame| {
            crate::workspaces::render_workspace_tabs(frame, frame.area(), &mut dashboard);
        })
        .unwrap();
    let screen = terminal.backend().to_string();
    assert!(screen.contains("Parent planning session"), "{screen}");
    assert!(screen.contains("X"), "{screen}");

    dashboard.close_subagent_workspace();
    assert_eq!(dashboard.subagent_parent_id(), None);
    assert_eq!(dashboard.selected_session_id(), Some(parent.id.as_str()));
}

/// A daemon-created child that arrives through a later runtime snapshot,
/// rather than through a full `Controller::load()`, must still hide from
/// the parent's real workspace. This is the contract the dashboard's
/// `apply_runtime_records` relies on when it assigns `state.subagents`
/// alongside `state.sessions` on every `set_state` call.
#[test]
fn a_second_set_state_with_a_new_relation_hides_the_new_child_too() {
    let mut parent = stopped_session();
    parent.id = "parent-session".into();
    parent.state = SessionState::Running;
    let mut first_child = stopped_session();
    first_child.id = "first-child".into();
    first_child.state = SessionState::Running;
    let first_relation = mj_core::subagent::SubagentRecord {
        child_session_id: first_child.id.clone(),
        parent_session_id: parent.id.clone(),
        task_name: "First task".into(),
        profile_id: first_child.last_profile.clone(),
        model: None,
        effort: None,
        working_directory: Default::default(),
        initial_prompt: "Do the first task".into(),
        request_key: "request-1".into(),
        created_at: first_child.created_at.clone(),
        noticed_turn: None,
    };
    let mut dashboard = DashboardState::new(
        config(),
        State {
            subagents: BTreeMap::from([(first_child.id.clone(), first_relation.clone())]),
            version: STATE_VERSION,
            sessions: BTreeMap::from([
                (parent.id.clone(), parent.clone()),
                (first_child.id.clone(), first_child.clone()),
            ]),
            mount_history: BTreeMap::new(),
            container_sizes: BTreeMap::new(),
        },
        BTreeMap::new(),
    );

    let mut second_child = stopped_session();
    second_child.id = "second-child".into();
    second_child.state = SessionState::Running;
    let second_relation = mj_core::subagent::SubagentRecord {
        child_session_id: second_child.id.clone(),
        parent_session_id: parent.id.clone(),
        task_name: "Second task".into(),
        profile_id: second_child.last_profile.clone(),
        model: None,
        effort: None,
        working_directory: Default::default(),
        initial_prompt: "Do the second task".into(),
        request_key: "request-2".into(),
        created_at: second_child.created_at.clone(),
        noticed_turn: None,
    };
    dashboard.set_state(State {
        subagents: BTreeMap::from([
            (first_child.id.clone(), first_relation),
            (second_child.id.clone(), second_relation),
        ]),
        version: STATE_VERSION,
        sessions: BTreeMap::from([
            (parent.id.clone(), parent.clone()),
            (first_child.id.clone(), first_child.clone()),
            (second_child.id.clone(), second_child.clone()),
        ]),
        mount_history: BTreeMap::new(),
        container_sizes: BTreeMap::new(),
    });

    assert_eq!(
        dashboard
            .ordered_sessions()
            .iter()
            .map(|session| session.id.as_str())
            .collect::<Vec<_>>(),
        vec![parent.id.as_str()],
        "a child arriving through set_state alone must still leave the real workspace"
    );

    dashboard.open_subagent_workspace(parent.id.clone());
    let mut virtual_workspace_ids = dashboard
        .ordered_sessions()
        .iter()
        .map(|session| session.id.clone())
        .collect::<Vec<_>>();
    virtual_workspace_ids.sort();
    assert_eq!(
        virtual_workspace_ids,
        vec![first_child.id.clone(), second_child.id.clone()],
        "both children must appear in the virtual workspace"
    );
}

#[test]
fn advanced_setting_reveals_only_stopped_sessions_in_the_selected_workspace() {
    let mut dashboard = dashboard_with_session(stopped_session());
    let mut live = running_session();
    live.id = "live".into();
    dashboard.state.sessions.insert(live.id.clone(), live);
    let mut lost = stopped_session();
    lost.id = "lost".into();
    lost.state = SessionState::Lost;
    dashboard.state.sessions.insert(lost.id.clone(), lost);
    let mut remote_history = stopped_session();
    remote_history.id = "remote-history".into();
    remote_history.workspace_id = "another-workspace".into();
    dashboard
        .state
        .sessions
        .insert(remote_history.id.clone(), remote_history);
    dashboard.clamp_selections();

    assert_eq!(
        dashboard
            .ordered_sessions()
            .iter()
            .map(|session| session.id.as_str())
            .collect::<Vec<_>>(),
        ["live"]
    );

    let mut config = dashboard.config.clone();
    config.advanced.show_stopped_sessions = true;
    dashboard.set_config(config);
    let visible = dashboard
        .ordered_sessions()
        .iter()
        .map(|session| session.id.as_str())
        .collect::<Vec<_>>();
    assert!(visible.contains(&"session-1"));
    assert!(visible.contains(&"live"));
    assert!(!visible.contains(&"lost"));
    assert!(!visible.contains(&"remote-history"));

    dashboard.select_active_session("session-1");
    let mut config = dashboard.config.clone();
    config.advanced.show_stopped_sessions = false;
    dashboard.set_config(config);
    assert_eq!(dashboard.selected_session_id(), Some("live"));
    assert!(dashboard.state.sessions.contains_key("session-1"));
}

#[test]
fn workspace_switch_restores_selection_and_pane_layout_without_losing_local_edits() {
    let mut local = running_session();
    local.id = "local".into();
    let mut remote = running_session();
    remote.id = "remote".into();
    remote.workspace_id = "remote-workspace".into();
    let mut dashboard = dashboard_with_session(local);
    dashboard.state.sessions.insert(remote.id.clone(), remote);
    dashboard.select_active_session("local");
    dashboard.set_pane_size(SupportPane::Sessions, PaneSize::Minimized);

    dashboard.cache_workspace_pane_sizes(
        "remote-workspace",
        PaneSizes {
            sessions: PaneSize::Maximized,
            targets: PaneSize::Standard,
            quota: PaneSize::Standard,
        },
    );
    dashboard.set_active_workspace(Some("remote-workspace".into()));
    dashboard.select_active_session("remote");
    assert_eq!(
        dashboard.pane_size(SupportPane::Sessions),
        PaneSize::Maximized
    );
    dashboard.set_pane_size(SupportPane::Sessions, PaneSize::Standard);
    dashboard.cache_workspace_pane_sizes(
        "remote-workspace",
        PaneSizes {
            sessions: PaneSize::Minimized,
            targets: PaneSize::Standard,
            quota: PaneSize::Standard,
        },
    );
    assert_eq!(
        dashboard.pane_size(SupportPane::Sessions),
        PaneSize::Standard
    );

    dashboard.set_active_workspace(Some(mj_core::workspace::DEFAULT_WORKSPACE_ID.into()));
    assert_eq!(dashboard.selected_session_id(), Some("local"));
    assert_eq!(
        dashboard.pane_size(SupportPane::Sessions),
        PaneSize::Minimized
    );
}

#[test]
fn sessions_are_ordered_by_creation_sequence_ascending() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.state.sessions.clear();
    for (id, created) in [
        ("session-z", "2026-08-09T01:00:00Z"),
        ("session-y", "2026-08-09T00:30:00-02:00"),
        ("session-a", "unknown"),
    ] {
        let mut session = running_session();
        session.id = id.into();
        session.created_at = created.into();
        dashboard.state.sessions.insert(id.into(), session);
    }
    assert_eq!(
        dashboard
            .ordered_sessions()
            .iter()
            .map(|s| s.id.as_str())
            .collect::<Vec<_>>(),
        ["session-z", "session-y", "session-a"]
    );
}

#[test]
fn resolved_git_origin_groups_differently_named_raw_worktrees() {
    let mut first = stopped_session();
    first.id = "bifrost-fird".into();
    first.state = SessionState::Running;
    first.project_directory = Some("/mnt/optane/bifrost-fird".into());
    let mut second = stopped_session();
    second.id = "bifrost-fuzz".into();
    second.state = SessionState::Running;
    second.project_directory = Some("/home/dev/bifrost-fuzz".into());
    let state = State {
        subagents: Default::default(),
        version: STATE_VERSION,
        sessions: [first, second]
            .into_iter()
            .map(|session| (session.id.clone(), session))
            .collect(),
        mount_history: BTreeMap::new(),
        container_sizes: BTreeMap::new(),
    };
    let mut dashboard = DashboardState::new(config(), state, BTreeMap::new());
    let source =
        ProjectSourceIdentity::git_remote("git@github.com:BrokkAi/bifrost-dev.git").unwrap();

    dashboard.set_project_source("bifrost-fird", source.clone());
    dashboard.set_project_source("bifrost-fuzz", source);

    assert_eq!(dashboard.project_keys(), ["github:brokkai/bifrost-dev"]);
    assert!(
        dashboard
            .ordered_sessions()
            .iter()
            .all(|session| dashboard.project_is_expanded(session))
    );
}

#[test]
fn bundle_and_checkout_share_one_canonical_project_heading() {
    let mut dashboard_config = config();
    dashboard_config.bundles.insert(
        "bifrost".into(),
        ProjectBundle {
            primary_repo: "bifrost".into(),
            repositories: vec![ProjectRepository {
                id: "bifrost".into(),
                github: Some("BrokkAi/bifrost-dev".into()),
                local: None,
                destination: "bifrost".into(),
                git_ref: None,
            }],
        },
    );
    let mut bundle_session = running_session();
    bundle_session.id = "bundle".into();
    bundle_session.bundle_id = "bifrost".into();
    assert!(bundle_session.project_directory.is_none());

    let single = DashboardState::new(
        dashboard_config.clone(),
        State {
            subagents: Default::default(),
            version: STATE_VERSION,
            sessions: [(bundle_session.id.clone(), bundle_session.clone())]
                .into_iter()
                .collect(),
            mount_history: BTreeMap::new(),
            container_sizes: BTreeMap::new(),
        },
        BTreeMap::new(),
    );
    let single_heading = single
        .sessions_rows()
        .into_iter()
        .find_map(|row| match row {
            SessionsRow::ProjectHeading { label, .. } => Some(label),
            SessionsRow::Session { .. } => None,
        })
        .expect("bundle project heading");
    assert_eq!(single_heading, "bifrost-dev");

    let mut raw_source = running_session();
    raw_source.id = "raw".into();
    raw_source.created_at = "2026-08-09T00:01:00Z".into();
    let mut dashboard = DashboardState::new(
        dashboard_config,
        State {
            subagents: Default::default(),
            version: STATE_VERSION,
            sessions: [bundle_session, raw_source]
                .into_iter()
                .map(|session| (session.id.clone(), session))
                .collect(),
            mount_history: BTreeMap::new(),
            container_sizes: BTreeMap::new(),
        },
        BTreeMap::new(),
    );
    dashboard.set_project_source(
        "raw",
        ProjectSourceIdentity::git_remote("git@github.com:BrokkAi/bifrost-dev.git")
            .expect("canonical source"),
    );

    let headings = dashboard
        .sessions_rows()
        .into_iter()
        .filter_map(|row| match row {
            SessionsRow::ProjectHeading { label, .. } => Some(label),
            SessionsRow::Session { .. } => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(dashboard.project_keys(), ["github:brokkai/bifrost-dev"]);
    assert_eq!(headings, ["bifrost-dev"]);
    assert_eq!(dashboard.ordered_sessions().len(), 2);

    // Case differences in a remote's spelling must not let another owner
    // split this canonical group into two headings during display sorting.
    dashboard.set_project_source(
        "raw",
        ProjectSourceIdentity::git_remote("https://github.com/brokkai/bifrost-dev.git").unwrap(),
    );
    let mut unrelated = running_session();
    unrelated.id = "unrelated".into();
    dashboard
        .state
        .sessions
        .insert(unrelated.id.clone(), unrelated);
    dashboard.set_project_source(
        "unrelated",
        ProjectSourceIdentity::git_remote("https://github.com/Else/bifrost-dev.git").unwrap(),
    );
    let keys = dashboard.project_keys();
    assert_eq!(keys.len(), 2);
    assert!(keys.contains(&"github:brokkai/bifrost-dev".to_owned()));
    assert!(keys.contains(&"github:else/bifrost-dev".to_owned()));
    assert_eq!(dashboard.ordered_sessions().len(), 3);
}

#[test]
fn mark_all_read_advances_a_materialized_session_and_returns_its_receipt() {
    let mut dashboard = dashboard_with_session(running_session());
    apply_materialized_transcript(&mut dashboard, vec![agent_message(4, "unread response")]);
    assert_eq!(
        dashboard.session_details["session-1"].unread_agent_messages,
        1
    );

    assert_eq!(
        dashboard.handle_key(alt_key('a')),
        DashboardAction::MarkAllRead {
            receipts: vec![("session-1".into(), 4)]
        }
    );
    assert_eq!(
        dashboard.session_details["session-1"].unread_agent_messages,
        0
    );
    assert_eq!(
        dashboard.state.sessions["session-1"].viewed_through_event_ordinal,
        4
    );
}

#[test]
fn mark_all_read_includes_a_restart_only_session() {
    let mut dashboard = dashboard_with_session(running_session());
    apply_materialized_transcript(&mut dashboard, vec![session_restart(3)]);
    assert_eq!(
        dashboard.session_details["session-1"].unread_session_restarts,
        1
    );

    assert_eq!(
        dashboard.handle_key(alt_key('a')),
        DashboardAction::MarkAllRead {
            receipts: vec![("session-1".into(), 3)]
        }
    );
    let detail = &dashboard.session_details["session-1"];
    assert_eq!(detail.unread_session_restarts, 0);
    assert!(!detail.has_unread());
}

#[test]
fn bracketed_paste_populates_dashboard_text_editors() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let mut dashboard = dashboard_with_session(session);

    open_rename_through_the_palette(&mut dashboard);
    let Mode::Rename(editor) = &mut dashboard.mode else {
        panic!("expected rename editor")
    };
    editor.title.clear();
    dashboard.handle_paste("pasted title\n");

    let Mode::Rename(editor) = &dashboard.mode else {
        panic!("expected rename editor")
    };
    assert_eq!(editor.title, "pasted title");
}

#[test]
fn the_tab_ring_visits_every_pane_and_keeps_the_session_selection() {
    let mut active = stopped_session();
    active.id = "session-0".into();
    active.state = SessionState::Running;
    let other = running_session();
    let mut dashboard = DashboardState::new(
        config(),
        State {
            subagents: Default::default(),
            version: STATE_VERSION,
            sessions: BTreeMap::from([(active.id.clone(), active), (other.id.clone(), other)]),
            mount_history: BTreeMap::new(),
            container_sizes: BTreeMap::new(),
        },
        BTreeMap::new(),
    );
    dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);

    assert_eq!(dashboard.focus, Focus::Sessions);
    assert_eq!(dashboard.selected_session().unwrap().id, "session-0");
    dashboard.handle_key(key(KeyCode::Down));
    assert_eq!(dashboard.selected_session().unwrap().id, "session-1");

    // The selection is anchored by session id, so it survives focus
    // moving away and Tab lands the user back where they were.
    for expected in [
        Focus::Prompt,
        Focus::Targets,
        Focus::Quota,
        Focus::Workspaces,
        Focus::Workspaces,
        Focus::Sessions,
    ] {
        dashboard.handle_key(key(KeyCode::Tab));
        assert_eq!(dashboard.focus, expected);
        assert_eq!(dashboard.selected_session().unwrap().id, "session-1");
    }

    for expected in [
        Focus::Workspaces,
        Focus::Workspaces,
        Focus::Quota,
        Focus::Targets,
        Focus::Prompt,
        Focus::Sessions,
    ] {
        dashboard.handle_key(key(KeyCode::BackTab));
        assert_eq!(dashboard.focus, expected);
    }
}

#[test]
fn keyboard_selection_stops_at_the_active_panes_ends_instead_of_wrapping() {
    let sessions = (0..3)
        .map(|index| {
            let mut session = stopped_session();
            session.id = format!("session-{index}");
            session.state = SessionState::Running;
            (session.id.clone(), session)
        })
        .collect();
    let mut dashboard = DashboardState::new(
        config(),
        State {
            subagents: Default::default(),
            version: STATE_VERSION,
            sessions,
            mount_history: BTreeMap::new(),
            container_sizes: BTreeMap::new(),
        },
        BTreeMap::new(),
    );

    assert_eq!(dashboard.selected_visible_index(), Some(0));
    dashboard.handle_key(key(KeyCode::Up));
    assert_eq!(
        dashboard.selected_visible_index(),
        Some(0),
        "Up to the actions preserves the selected conversation"
    );
    assert_eq!(
        dashboard.session_action_focus,
        Some(CommandId::NewSessionWizard)
    );
    dashboard.handle_key(key(KeyCode::Down));
    assert_eq!(dashboard.session_action_focus, None);
    assert_eq!(dashboard.selected_visible_index(), Some(0));

    dashboard.handle_key(key(KeyCode::Down));
    dashboard.handle_key(key(KeyCode::Down));
    assert_eq!(dashboard.selected_visible_index(), Some(2));
    dashboard.handle_key(key(KeyCode::Down));
    assert_eq!(
        dashboard.selected_visible_index(),
        Some(2),
        "Down at the last row stays put"
    );
}

/// Every project starts expanded, and a collapsed one is still a list of
/// sessions: Enter opens the row under the caret rather than spending the
/// key on the group.
#[test]
fn enter_opens_the_selected_session_even_inside_a_collapsed_project() {
    let sessions = (0..3)
        .map(|index| {
            let mut session = stopped_session();
            session.id = format!("session-{index}");
            session.state = SessionState::Running;
            session.project_directory = Some(if index < 2 {
                "/projects/shared".into()
            } else {
                "/projects/other".into()
            });
            session.created_at = format!("2026-08-1{}T00:00:00Z", index + 1);
            (session.id.clone(), session)
        })
        .collect();
    let mut dashboard = DashboardState::new(
        config(),
        State {
            subagents: Default::default(),
            version: STATE_VERSION,
            sessions,
            mount_history: BTreeMap::new(),
            container_sizes: BTreeMap::new(),
        },
        BTreeMap::new(),
    );
    dashboard.select_active_session("session-1");
    assert!(
        dashboard.project_is_expanded(dashboard.selected_session().unwrap()),
        "projects default to expanded"
    );

    // Collapsing the selected session's project leaves the selection, and
    // Enter still opens it.
    dashboard.handle_key(key(KeyCode::Char(' ')));
    assert!(!dashboard.project_is_expanded(dashboard.selected_session().unwrap()));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::Open {
            session_id: "session-1".into()
        }
    );
    assert_eq!(dashboard.selected_session().unwrap().id, "session-1");
}

#[test]
fn mouse_wheel_scrolls_the_hovered_pane_without_changing_focus() {
    let sessions = (0..5)
        .map(|index| {
            let mut session = stopped_session();
            session.id = format!("session-{index}");
            session.state = SessionState::Running;
            (session.id.clone(), session)
        })
        .collect();
    let mut dashboard = DashboardState::new(
        config(),
        State {
            subagents: Default::default(),
            version: STATE_VERSION,
            sessions,
            mount_history: BTreeMap::new(),
            container_sizes: BTreeMap::new(),
        },
        BTreeMap::new(),
    );
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("test terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw pane hitboxes");
    let pane_areas = dashboard.pane_areas.expect("dashboard pane hitboxes");

    // The sessions pane moves one row per wheel notch, like a single arrow.
    dashboard.handle_mouse(mouse_in(MouseEventKind::ScrollDown, pane_areas[0]));
    assert_eq!(dashboard.selected_visible_index().unwrap_or(0), 1);
    dashboard.handle_mouse(mouse_in(MouseEventKind::ScrollUp, pane_areas[0]));
    assert_eq!(dashboard.selected_visible_index().unwrap_or(0), 0);

    // The quota pane is also a list: one notch moves its selection by one.
    assert_eq!(dashboard.focus, Focus::Sessions);
    dashboard.handle_mouse(mouse_in(MouseEventKind::ScrollDown, pane_areas[2]));
    assert_eq!(dashboard.quota_index, 1);
    assert_eq!(dashboard.selected_visible_index().unwrap_or(0), 0);
    assert_eq!(dashboard.focus, Focus::Sessions);
}

#[test]
fn clicking_an_active_rows_tail_line_selects_that_session() {
    let mut dashboard = dashboard_with_conversations(3);
    dashboard.set_pane_size(SupportPane::Sessions, PaneSize::Maximized);
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("test terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw active row hitboxes");
    assert_eq!(
        dashboard.selected_visible_index().unwrap_or(0),
        0,
        "starts on the first session"
    );

    let (_, row) = *dashboard
        .session_row_areas
        .iter()
        .find(|(index, _)| *index == 2)
        .expect("the third active row has a recorded hitbox");
    assert!(
        row.height > 1,
        "an unselected row still spans its preview lines"
    );
    // Click the row's bottom line, i.e. its conversation tail, not the
    // one-line summary at the top.
    dashboard.handle_mouse(mouse_at_row(
        MouseEventKind::Down(MouseButton::Left),
        row,
        row.height - 1,
    ));

    assert_eq!(
        dashboard.selected_visible_index().unwrap_or(0),
        2,
        "clicking the tail line selected the row, not just its summary line"
    );
    assert_eq!(dashboard.focus, Focus::Sessions);
}

#[test]
fn a_single_click_on_a_row_selects_but_reports_no_action() {
    let mut dashboard = dashboard_with_conversations(3);
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("test terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw active row hitboxes");

    let (_, row) = *dashboard
        .session_row_areas
        .iter()
        .find(|(index, _)| *index == 1)
        .expect("the second active row has a recorded hitbox");
    let action = dashboard.handle_mouse(mouse_at_row(
        MouseEventKind::Down(MouseButton::Left),
        row,
        0,
    ));

    assert_eq!(action, DashboardAction::None);
    assert_eq!(dashboard.selected_visible_index().unwrap_or(0), 1);
}

#[test]
fn a_double_click_on_an_active_row_opens_it_like_enter() {
    let mut dashboard = dashboard_with_conversations(3);
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("test terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw active row hitboxes");

    let (_, row) = *dashboard
        .session_row_areas
        .iter()
        .find(|(index, _)| *index == 1)
        .expect("the second active row has a recorded hitbox");
    let click = || mouse_at_row(MouseEventKind::Down(MouseButton::Left), row, 0);

    let first = dashboard.handle_mouse(click());
    assert_eq!(first, DashboardAction::None, "the first click just selects");

    let second = dashboard.handle_mouse(click());
    assert_eq!(
        second,
        DashboardAction::Open {
            session_id: "session-1".into(),
        },
        "a quick second click on the same row opens it, matching Enter"
    );
    assert_eq!(dashboard.selected_visible_index().unwrap_or(0), 1);
}

#[test]
fn clicks_on_different_rows_do_not_count_as_a_double_click() {
    let mut dashboard = dashboard_with_conversations(3);
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("test terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw active row hitboxes");

    let row_for = |index: usize| {
        *dashboard
            .session_row_areas
            .iter()
            .find(|(row_index, _)| *row_index == index)
            .map(|(_, area)| area)
            .expect("row has a recorded hitbox")
    };
    let first_row = row_for(0);
    let second_row = row_for(1);

    let first = dashboard.handle_mouse(mouse_at_row(
        MouseEventKind::Down(MouseButton::Left),
        first_row,
        0,
    ));
    assert_eq!(first, DashboardAction::None);

    // A click on a different row is a fresh first click, not the second
    // half of a double click on row 0.
    let second = dashboard.handle_mouse(mouse_at_row(
        MouseEventKind::Down(MouseButton::Left),
        second_row,
        0,
    ));
    assert_eq!(second, DashboardAction::None);
    assert_eq!(
        dashboard.selected_visible_index().unwrap_or(0),
        1,
        "the second click's row is selected"
    );
}

/// A dashboard with `count` running sessions, each carrying a numbered
/// conversation long enough to scroll.
fn dashboard_with_conversations(count: usize) -> DashboardState {
    let sessions = (0..count)
        .map(|index| {
            let mut session = stopped_session();
            session.id = format!("session-{index}");
            session.state = SessionState::Running;
            (session.id.clone(), session)
        })
        .collect();
    let mut dashboard = DashboardState::new(
        config(),
        State {
            subagents: Default::default(),
            version: STATE_VERSION,
            sessions,
            mount_history: BTreeMap::new(),
            container_sizes: BTreeMap::new(),
        },
        BTreeMap::new(),
    );
    let transcript = numbered_conversation(14);
    for index in 0..count {
        apply_materialized_transcript_for(
            &mut dashboard,
            &format!("session-{index}"),
            transcript.clone(),
        );
    }
    dashboard
}

#[test]
fn newly_ready_session_can_be_selected_after_state_refresh() {
    let mut new_session = stopped_session();
    new_session.id = "new-session".into();
    new_session.state = SessionState::Running;
    let mut other = stopped_session();
    other.id = "other".into();
    other.state = SessionState::Running;
    let mut dashboard = DashboardState::new(
        config(),
        State {
            subagents: Default::default(),
            version: STATE_VERSION,
            sessions: BTreeMap::from([(other.id.clone(), other)]),
            mount_history: BTreeMap::new(),
            container_sizes: BTreeMap::new(),
        },
        BTreeMap::new(),
    );
    dashboard.focus = Focus::Quota;

    let mut refreshed = dashboard.state.clone();
    refreshed
        .sessions
        .insert(new_session.id.clone(), new_session);
    dashboard.set_state(refreshed);
    dashboard.select_active_session("new-session");

    // Selecting a session no longer steals the keyboard: the caller
    // decides where focus belongs, so a background arrival cannot pull it
    // out of the composer.
    assert_eq!(dashboard.focus, Focus::Quota);
    assert_eq!(dashboard.selected_session().unwrap().id, "new-session");
}

/// Stopping the last session empties the live dashboard; it belongs to the
/// resume dialog now.
#[test]
fn stopping_the_last_session_removes_it_and_panes_still_cycle() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let mut dashboard = dashboard_with_session(session);
    dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
    assert_eq!(dashboard.focus, Focus::Sessions);

    let mut state = dashboard.state.clone();
    state.sessions.get_mut("session-1").unwrap().state = SessionState::Stopped;
    dashboard.set_state(state);
    assert_eq!(dashboard.focus, Focus::Sessions);
    assert_eq!(dashboard.ordered_sessions().len(), 0);
    assert_eq!(dashboard.selected_session(), None);

    for expected in [
        Focus::Prompt,
        Focus::Targets,
        Focus::Quota,
        Focus::Workspaces,
        Focus::Workspaces,
        Focus::Sessions,
    ] {
        dashboard.handle_key(key(KeyCode::Tab));
        assert_eq!(dashboard.focus, expected);
    }
}

#[test]
fn opening_an_active_session_returns_controller_action() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    session.checkpoint = None;
    let mut dashboard = dashboard_with_session(session);
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::Open {
            session_id: "session-1".into()
        }
    );
}

/// A failed session has two reasonable answers to Enter, and recovery
/// replaces the target, so the surface asks instead of guessing. The row
/// is red before the key is ever pressed, so the dialog is not a surprise.
#[test]
fn enter_on_a_failed_session_offers_recovery_and_the_transcript() {
    let mut session = stopped_session();
    session.state = SessionState::Error;
    session.last_error = Some("worker bootstrap failed: upload failed".into());
    let mut dashboard = dashboard_with_session(session);

    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    let Mode::Confirm(dialog) = &dashboard.mode else {
        panic!(
            "expected the failed-session prompt, got {:?}",
            dashboard.mode
        )
    };
    assert_eq!(
        dialog.confirmation,
        Confirmation::RecoverFailed {
            session_id: "session-1".into(),
            error: Some("worker bootstrap failed: upload failed".into()),
            recoverable: true,
        }
    );

    // The prompt draws, names what failed, and offers both answers.
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).expect("terminal");
    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .expect("draw the failed-session prompt");
    let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
    assert!(rendered.contains("Session failed"), "{rendered}");
    assert!(rendered.contains("worker bootstrap failed"), "{rendered}");
    assert!(rendered.contains("Open transcript"), "{rendered}");
    assert!(rendered.contains("Recover"), "{rendered}");

    // Reading what it did changes nothing about the session.
    dashboard.handle_key(key(KeyCode::Left));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::Open {
            session_id: "session-1".into()
        }
    );
    assert_eq!(dashboard.focus, Focus::Prompt);
}

/// Recovery is only on offer when there is a verified copy to recover
/// from; without one the prompt says so rather than showing a button that
/// cannot work.
#[test]
fn a_failed_session_without_a_recovery_copy_is_not_offered_recovery() {
    let mut session = stopped_session();
    session.state = SessionState::Error;
    session.checkpoint = None;
    session.last_error = Some("worker bootstrap failed".into());
    let mut dashboard = dashboard_with_session(session);

    dashboard.handle_key(key(KeyCode::Enter));
    let Mode::Confirm(dialog) = &dashboard.mode else {
        panic!("expected the failed-session prompt")
    };
    assert_eq!(
        dialog.confirmation,
        Confirmation::RecoverFailed {
            session_id: "session-1".into(),
            error: Some("worker bootstrap failed".into()),
            recoverable: false,
        }
    );
}
