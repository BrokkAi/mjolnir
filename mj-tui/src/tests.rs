use std::collections::BTreeMap;

use crossterm::event::{Event, KeyCode, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use mj_core::config::{ProjectBundle, ProjectRepository};
use mj_core::state::{
    MaterializedExecutionState, STATE_VERSION, SessionState, State, TranscriptBody,
};

use super::*;
use crate::test_support::*;

use crate::render::render;

#[test]
fn retained_subagents_do_not_count_as_working_after_profile_move() {
    use mj_core::native_agent::*;
    let mut parent = running_session();
    let mut dashboard = dashboard_with_session(parent.clone());
    let agents: Vec<_> = (0..23)
        .map(|i| {
            let agent = NativeAgent {
                owner_session_id: parent.id.clone(),
                session_id: format!("child-{i}"),
                parent_session_id: None,
                name: format!("Child {i}"),
                task: "Inspect code".into(),
                capabilities: NativeAgentCapabilities::default(),
                state: if i < 17 {
                    NativeAgentState::Completed
                } else {
                    NativeAgentState::Disconnected
                },
                availability: NativeAgentAvailability::Unknown,
                availability_reason: None,
                stable_id: None,
            };
            NativeAgentView {
                generation_ordinal: 1,
                projection: mj_core::state::MaterializedSession::empty(agent.view_id()),
                agent,
            }
        })
        .collect();
    dashboard.set_native_agents(agents.clone());
    parent.last_profile = "destination-profile".into();
    let mut state = State::default();
    state.sessions.insert(parent.id.clone(), parent.clone());
    dashboard.set_state(state);
    assert_eq!(dashboard.subagent_count_for(&parent.id), 23);
    assert_eq!(dashboard.working_subagent_count_for(&parent.id), 0);
    dashboard.open_subagent_workspace(parent.id.clone());
    assert_eq!(dashboard.ordered_sessions().len(), 23);
    let mut resumed = agents;
    resumed[0].agent.state = NativeAgentState::Running;
    resumed[0].agent.availability = NativeAgentAvailability::Available;
    dashboard.set_native_agents(resumed);
    assert_eq!(dashboard.working_subagent_count_for(&parent.id), 1);
}

#[test]
fn inert_pointer_motion_is_not_consumed() {
    let mut dashboard = dashboard_with_session(running_session());
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .unwrap();

    for column in 0..120 {
        let result = dashboard.handle_event_result(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column,
            row: 1,
            modifiers: KeyModifiers::NONE,
        }));
        assert!(!result.consumed, "pointer motion belongs to the selection");
    }
}

#[test]
fn a_clamped_selection_move_is_still_consumed() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.select_active_session("session-1");

    let result = dashboard.handle_event_result(Event::Key(key(KeyCode::Down)));

    assert!(result.consumed);
    assert!(result.action.is_none());
}

#[test]
fn activating_target_rename_opens_the_config_id_editor() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.begin_target_actions();
    dashboard.handle_event_result(Event::Key(key(KeyCode::Tab)));

    let result = dashboard.handle_event_result(Event::Key(key(KeyCode::Enter)));

    assert!(matches!(dashboard.mode, Mode::ConfigId(_)));
    assert!(result.consumed);
}

#[test]
fn a_cursor_only_text_edit_is_consumed_without_an_action() {
    let mut session = running_session();
    session.session_title_override = Some("rename me".into());
    let mut dashboard = dashboard_with_session(session);
    dashboard.select_active_session("session-1");
    dashboard.dispatch_command(CommandId::RenameSession);
    assert!(matches!(dashboard.mode, Mode::Rename(_)));

    let result = dashboard.handle_event_result(Event::Key(crossterm::event::KeyEvent::new(
        KeyCode::Left,
        KeyModifiers::NONE,
    )));

    assert!(result.consumed);
    assert!(result.action.is_none());
}

/// Opens the rename editor the way the surface offers it now: the palette
/// chord, type enough of "rename" to pick it out, Enter. There is no `e` any
/// more.
fn open_rename_through_the_palette(dashboard: &mut DashboardState) {
    dashboard.focus_sessions();
    open_palette(dashboard);
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
        chord(&mut dashboard, CommandId::ResumeDialog),
        DashboardAction::OpenResumeDialog
    );
    dashboard.cancel_modal();
    assert_eq!(
        open_new_session_wizard(&mut dashboard),
        DashboardAction::None
    );
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

    assert_eq!(DashboardAction::None, DashboardAction::None);
    assert_eq!(dashboard.mode, Mode::Dashboard);
    assert_eq!(
        dashboard.dispatch_command(CommandId::Workspaces),
        DashboardAction::LoadWorkspaceManagement { generation: 1 }
    );
    dashboard.cancel_modal();
    assert_eq!(
        chord(&mut dashboard, CommandId::WebViewer),
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

    // The refresh chord answers from every pane.
    assert_eq!(
        chord(&mut dashboard, CommandId::Refresh),
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

    assert_eq!(
        chord(&mut dashboard, CommandId::TogglePanePreset),
        DashboardAction::None
    );
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

    assert_eq!(
        chord(&mut dashboard, CommandId::TogglePanePreset),
        DashboardAction::None
    );
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
    chord(&mut dashboard, CommandId::CycleFocusedPaneSize);
    assert_eq!(
        dashboard.pane_size(SupportPane::Sessions),
        PaneSize::Maximized
    );
    assert_eq!(dashboard.focus, Focus::Sessions);

    dashboard.focus = Focus::Targets;
    chord(&mut dashboard, CommandId::CycleFocusedPaneSize);
    assert_eq!(
        dashboard.pane_size(SupportPane::Targets),
        PaneSize::Maximized
    );
    assert_eq!(
        dashboard.pane_size(SupportPane::Sessions),
        PaneSize::Standard
    );
    assert_eq!(dashboard.pane_size(SupportPane::Quota), PaneSize::Standard);

    chord(&mut dashboard, CommandId::CycleFocusedPaneSize);
    assert_eq!(
        dashboard.pane_size(SupportPane::Targets),
        PaneSize::Minimized
    );
    chord(&mut dashboard, CommandId::CycleFocusedPaneSize);
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
    chord(&mut dashboard, CommandId::CycleFocusedPaneSize);
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
    chord(&mut dashboard, CommandId::CycleFocusedPaneSize);
    assert_eq!(
        dashboard.notice().as_deref(),
        Some("Select Sessions, Targets, or Profiles before cycling the pane size.")
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
    assert_eq!(
        open_new_session_wizard(&mut dashboard),
        DashboardAction::None
    );
    assert!(matches!(dashboard.mode, Mode::New(_)));
    dashboard.cancel_modal();

    assert_eq!(
        chord(&mut dashboard, CommandId::ResumeDialog),
        DashboardAction::OpenResumeDialog
    );
    assert_eq!(
        chord(&mut dashboard, CommandId::MarkAllRead),
        DashboardAction::None
    );
    assert_eq!(
        dashboard.notice().as_deref(),
        Some("No unread sessions in this workspace.")
    );
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
    assert_eq!(
        open_new_session_wizard(&mut new_session),
        DashboardAction::None
    );
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

/// The combined surface is quit with the detach chord. A stray Escape must never
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
        chord(&mut dashboard, CommandId::CancelOperation),
        DashboardAction::CancelOperation {
            session_id,
            kind: SessionOperationKind::Launching,
        }
    );
}

/// Launch campaign finding A-7: the cancel chord with nothing in flight
/// says so instead of doing nothing silently.
#[test]
fn cancel_with_nothing_in_flight_shows_a_notice() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.focus_sessions();
    assert_eq!(
        chord(&mut dashboard, CommandId::CancelOperation),
        DashboardAction::None
    );
    assert_eq!(dashboard.notice().as_deref(), Some("Nothing to cancel"));
}

/// Launch campaign finding A-10: a pinned pane restored for a suspended
/// session kept trying to attach and failed after 15 seconds with a notice
/// that named no session. The pane is emptied instead, with a notice that
/// names the session; a failure to open names it too.
#[test]
fn a_pinned_pane_releases_a_suspended_session_and_names_it() {
    let mut session = stopped_session();
    session.session_title_override = Some("Pinned B".into());
    let mut dashboard = dashboard_with_session(session);
    let pane = dashboard.browse_pane();
    dashboard
        .split_focused_pane(ratatui::layout::Direction::Horizontal, None)
        .unwrap();
    dashboard.set_pane_session(pane, Some("session-1"));
    assert!(dashboard.pane_session_is_suspended("session-1"));
    dashboard.release_suspended_pane(pane, "session-1");
    assert_eq!(dashboard.pane_session(pane), None);
    let notice = dashboard.notice().unwrap();
    assert!(notice.contains("Session Pinned B is suspended"), "{notice}");

    dashboard.report_open_failure(
        "session-1",
        "Session opening did not respond within 15 seconds",
    );
    let notice = dashboard.notice().unwrap();
    assert!(
        notice.starts_with("Could not open Session Pinned B: "),
        "{notice}"
    );

    // A session being resumed is on its way back and is not released.
    dashboard.begin_session_operation("session-1".into(), SessionOperationKind::Resuming, None);
    assert!(!dashboard.pane_session_is_suspended("session-1"));
}

/// Launch campaign finding B-10: the title is a record field, not worker
/// state, so a session that is still starting can be renamed from the
/// Sessions pane and from its type-ahead composer.
#[test]
fn a_starting_session_can_be_renamed() {
    for from_prompt in [false, true] {
        let mut session = stopped_session();
        session.state = SessionState::Provisioning;
        let mut dashboard = dashboard_with_session(session);
        dashboard.begin_session_operation(
            "session-1".into(),
            SessionOperationKind::Launching,
            None,
        );
        if from_prompt {
            // The launch leaves the conversation pane empty until the attach
            // finishes, with the keyboard in the composer.
            dashboard.pane_sessions.clear();
            dashboard.focus_prompt();
        } else {
            dashboard.focus_sessions();
        }
        chord(&mut dashboard, CommandId::RenameSession);
        let Mode::Rename(editor) = &dashboard.mode else {
            panic!(
                "from_prompt={from_prompt}: {:?} notice={:?}",
                dashboard.mode,
                dashboard.notice()
            );
        };
        assert_eq!(editor.session_id, "session-1");
    }
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

/// Enter during a starting transition hands the prompt to the host for
/// delivery: the draft clears and the text stays visible as a queued preview.
#[test]
fn enter_during_a_starting_transition_queues_the_prompt_for_delivery() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let mut dashboard = dashboard_with_session(session);
    dashboard.begin_session_operation("session-1".into(), SessionOperationKind::Launching, None);
    dashboard.focus_prompt();
    dashboard.handle_key(key(KeyCode::Char('h')));

    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::QueueStartupPrompt {
            session_id: "session-1".into(),
            text: "h".into(),
        }
    );
    assert_eq!(
        dashboard
            .standby_prompts
            .get("session-1")
            .map(|standby| standby.draft()),
        Some(String::new())
    );
    assert_eq!(
        dashboard
            .standby_prompts
            .get("session-1")
            .map(|standby| standby.queued_prompt_texts()),
        Some(vec!["h".to_owned()])
    );
}

/// A prompt the daemon refused comes back as the draft, ahead of anything
/// typed since, and its preview goes away.
#[test]
fn a_refused_startup_prompt_returns_to_the_standby_draft() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let mut dashboard = dashboard_with_session(session);
    dashboard.begin_session_operation("session-1".into(), SessionOperationKind::Launching, None);
    dashboard.focus_prompt();
    for character in "first".chars() {
        dashboard.handle_key(key(KeyCode::Char(character)));
    }
    dashboard.handle_key(key(KeyCode::Enter));
    for character in "next".chars() {
        dashboard.handle_key(key(KeyCode::Char(character)));
    }

    dashboard.restore_standby_prompt("session-1", "first");

    let standby = dashboard
        .standby_prompts
        .get("session-1")
        .expect("standby composer");
    assert_eq!(standby.draft(), "first\nnext");
    assert!(standby.queued_prompt_texts().is_empty());
}

/// A command cannot be answered while the session is offline, so Enter keeps
/// the draft and explains instead of queueing anything.
#[test]
fn a_command_in_the_standby_composer_keeps_its_draft() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let mut dashboard = dashboard_with_session(session);
    dashboard.begin_session_operation("session-1".into(), SessionOperationKind::Launching, None);
    dashboard.focus_prompt();
    for character in "/help".chars() {
        dashboard.handle_key(key(KeyCode::Char(character)));
    }

    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );

    let standby = dashboard
        .standby_prompts
        .get("session-1")
        .expect("standby composer");
    assert!(standby.notice().is_some());
    assert!(dashboard.notice().is_none());
    assert_eq!(standby.draft(), "/help");
    assert!(standby.queued_prompt_texts().is_empty());
}

/// A creation that has not registered yet has no session to key a standby by,
/// so the launch standby takes the typing instead of the session that was
/// selected before. Enter there keeps the text as a preview, because there is
/// no session id to queue it against yet.
#[test]
fn typing_before_a_launch_registers_edits_the_launch_standby() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let mut dashboard = dashboard_with_session(session);
    dashboard.begin_launch_standby(mj_chat::chat::SessionHeaderIdentity {
        target: "/tmp/project".into(),
        profile: "profile-1".into(),
        title: String::new(),
        harness_kind: None,
        subagent_count: 0,
    });

    for character in "hello".chars() {
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Char(character))),
            DashboardAction::None
        );
    }
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    dashboard.handle_paste("later");

    assert!(dashboard.has_launch_standby());
    assert!(dashboard.launch_standby_capturing());
    assert!(dashboard.standby_prompts.is_empty());
    let standby = dashboard.launch_standby.as_ref().expect("launch standby");
    assert_eq!(standby.draft(), "later");
    assert_eq!(standby.queued_prompt_texts(), vec!["hello".to_owned()]);
}

/// Registration hands the launch standby to the new session: the draft and the
/// queued previews move into that session's standby, and the queued texts come
/// back oldest first so the host can have the daemon deliver each one.
#[test]
fn adopting_the_launch_standby_moves_its_draft_and_prompts() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let mut dashboard = dashboard_with_session(session);
    dashboard.begin_launch_standby(mj_chat::chat::SessionHeaderIdentity {
        target: "/tmp/project".into(),
        profile: "profile-1".into(),
        title: String::new(),
        harness_kind: None,
        subagent_count: 0,
    });
    for character in "first".chars() {
        dashboard.handle_key(key(KeyCode::Char(character)));
    }
    dashboard.handle_key(key(KeyCode::Enter));
    for character in "second".chars() {
        dashboard.handle_key(key(KeyCode::Char(character)));
    }
    dashboard.handle_key(key(KeyCode::Enter));
    for character in "still typing".chars() {
        dashboard.handle_key(key(KeyCode::Char(character)));
    }

    assert_eq!(
        dashboard.adopt_launch_standby("session-1"),
        vec!["first".to_owned(), "second".to_owned()]
    );

    assert!(!dashboard.has_launch_standby());
    assert!(!dashboard.launch_standby_capturing());
    let standby = dashboard
        .standby_prompts
        .get("session-1")
        .expect("adopted standby composer");
    assert_eq!(standby.draft(), "still typing");
    assert_eq!(
        standby.queued_prompt_texts(),
        vec!["first".to_owned(), "second".to_owned()]
    );
}

/// Selecting another session while a launch is being prepared hands the keys
/// back to that session, and the launch standby keeps its text for the session
/// that is still on its way.
#[test]
fn selecting_a_session_stops_the_launch_standby_from_capturing() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let mut dashboard = dashboard_with_session(session);
    dashboard.begin_launch_standby(mj_chat::chat::SessionHeaderIdentity {
        target: "/tmp/project".into(),
        profile: "profile-1".into(),
        title: String::new(),
        harness_kind: None,
        subagent_count: 0,
    });
    dashboard.handle_key(key(KeyCode::Char('h')));

    dashboard.selected_session_id = Some("session-other".into());

    assert!(dashboard.has_launch_standby());
    assert!(!dashboard.launch_standby_capturing());
    assert_eq!(
        dashboard
            .launch_standby
            .as_ref()
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
    dashboard.begin_session_operation("session-1".into(), SessionOperationKind::Suspending, None);
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

    assert_eq!(
        chord(&mut dashboard, CommandId::MarkAllRead),
        DashboardAction::None
    );
    assert_eq!(
        dashboard.notice().as_deref(),
        Some("No unread sessions in this workspace.")
    );
}

#[test]
fn the_detach_chord_quits_without_mutating_any_dashboard_modal() {
    let mut new_session = DashboardState::new(config(), State::default(), BTreeMap::new());
    assert_eq!(
        open_new_session_wizard(&mut new_session),
        DashboardAction::None
    );

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
            delete_branch_available: false,
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

        // Detach answers from every surface: the event loop routes it before
        // the mode underneath sees the key, so this drives that same path.
        assert!(
            dashboard.command_allowed_now(CommandId::QuitDetach),
            "{label}"
        );
        assert_eq!(
            chord(&mut dashboard, CommandId::QuitDetach),
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
    parent.project_directory = Some(std::path::PathBuf::from("/worktrees/parent-session"));
    parent.managed_worktree = Some(mj_core::state::ManagedWorktree {
        kind: Default::default(),
        source_project_directory: std::path::PathBuf::from("/src/mjolnir-main"),
        source_repository: std::path::PathBuf::from("/src/mjolnir-main"),
        worktree_root: std::path::PathBuf::from("/worktrees/parent-session"),
        branch: "mj/parent-session".into(),
        target: mj_core::state::ManagedWorktreeTarget::Local,
        base_commit: None,
    });
    let mut child = stopped_session();
    child.id = "child-session".into();
    child.title = "Inspect the parser and report back".into();
    child.session_title_override = Some("Inspect parser".into());
    child.acp_session_title = None;
    child.state = SessionState::Running;
    // A child launches into the parent's worktree checkout and owns no
    // worktree of its own, so its own project identity is the parent's
    // session id.
    child.project_directory = Some(std::path::PathBuf::from("/worktrees/parent-session"));
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
        handback_tool: false,
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

    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 12)).unwrap();
    terminal
        .draw(|frame| {
            crate::render::render_sessions(frame, frame.area(), &dashboard);
        })
        .unwrap();
    let rows = terminal.backend().to_string();
    assert!(rows.contains("Inspect parser"), "{rows}");
    assert!(
        rows.contains("mjolnir-main"),
        "a child groups under the project its parent works in: {rows}"
    );
    assert!(
        !rows.contains("parent-session"),
        "no row names the parent session id: {rows}"
    );

    dashboard.close_subagent_workspace();
    assert_eq!(dashboard.subagent_parent_id(), None);
    assert_eq!(dashboard.selected_session_id(), Some(parent.id.as_str()));
}

/// A stopped Mjolnir sub-agent has no worker to attach to. Found live: its
/// conversation sat on "Bringing your conversation into focus" until the
/// attach timed out, and the work it did could not be read. It opens as a
/// read-only view of its stored transcript instead.
#[test]
fn a_stopped_subagent_opens_as_its_stored_read_only_transcript() {
    let mut parent = stopped_session();
    parent.id = "parent-session".into();
    parent.state = SessionState::Running;
    let mut child = stopped_session();
    child.id = "child-session".into();
    child.session_title_override = Some("Inspect parser".into());
    child.state = SessionState::Stopped;
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
        handback_tool: true,
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
    assert!(dashboard.is_stopped_subagent(&child.id));
    assert!(!dashboard.is_stopped_subagent(&parent.id));
    // The host empties a pane whose session this calls suspended, which
    // would take the read-only view away as soon as Browse showed it.
    assert!(!dashboard.pane_session_is_suspended(&child.id));

    dashboard.open_subagent_workspace(parent.id.clone());
    assert_eq!(
        dashboard.open_selected_session(),
        DashboardAction::Open {
            session_id: child.id.clone()
        },
        "Enter reads a stopped child rather than offering to resume it"
    );
    assert!(
        dashboard.begin_stopped_subagent(&child.id),
        "first open loads it"
    );
    assert!(
        !dashboard.begin_stopped_subagent(&child.id),
        "a second open does not load it again"
    );

    let draw = |dashboard: &mut DashboardState| {
        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                let transcript = ratatui::layout::Rect::new(0, 0, area.width, 12);
                let prompt = ratatui::layout::Rect::new(0, 12, area.width, 4);
                dashboard.render_stopped_subagent(frame, "child-session", transcript, prompt);
            })
            .unwrap();
        terminal.backend().to_string()
    };
    assert!(draw(&mut dashboard).contains("Loading this sub-agent"));

    let mut stored = mj_core::state::MaterializedSession::empty(child.id.clone());
    stored.transcript = vec![crate::test_support::agent_message(
        3,
        "Handed back: the parser drops trailing commas.",
    )];
    dashboard.set_stopped_subagent_transcript(&child.id, Ok(Some(stored)));
    let screen = draw(&mut dashboard);
    assert!(screen.contains("Inspect parser · stopped"), "{screen}");
    assert!(screen.contains("parser drops trailing commas"), "{screen}");
    assert!(screen.contains("read-only"), "{screen}");

    // A failed read says why and loads again on the next open.
    dashboard.set_stopped_subagent_transcript(&child.id, Err("store is busy".into()));
    assert!(draw(&mut dashboard).contains("store is busy"));
    assert!(dashboard.begin_stopped_subagent(&child.id));
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
        handback_tool: false,
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
        handback_tool: false,
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
        chord(&mut dashboard, CommandId::MarkAllRead),
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
fn mark_all_read_includes_an_interruption_only_session() {
    let mut dashboard = dashboard_with_session(running_session());
    apply_materialized_transcript(&mut dashboard, vec![work_interruption(3)]);
    assert_eq!(
        dashboard.session_details["session-1"].unread_interruptions,
        1
    );

    assert_eq!(
        chord(&mut dashboard, CommandId::MarkAllRead),
        DashboardAction::MarkAllRead {
            receipts: vec![("session-1".into(), 3)]
        }
    );
    let detail = &dashboard.session_details["session-1"];
    assert_eq!(detail.unread_interruptions, 0);
    assert!(!detail.has_unread());
}

#[test]
fn mark_all_read_leaves_another_workspaces_session_unread() {
    let mut dashboard = dashboard_with_attention_mix();
    dashboard.set_active_workspace(Some("default".into()));
    for id in ["done", "remote"] {
        apply_materialized_transcript_for(
            &mut dashboard,
            id,
            vec![agent_message(4, "unread response")],
        );
        assert!(dashboard.session_details[id].has_unread(), "{id}");
    }

    assert_eq!(
        chord(&mut dashboard, CommandId::MarkAllRead),
        DashboardAction::MarkAllRead {
            receipts: vec![("done".into(), 4)]
        }
    );
    assert!(
        dashboard.session_details["remote"].has_unread(),
        "a session in another workspace keeps its unread marker"
    );
    assert_eq!(
        dashboard.state.sessions["remote"].viewed_through_event_ordinal,
        0
    );
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

/// Two running sessions in the active workspace, so a pane can be given one
/// of them and a split the other.
fn dashboard_with_two_sessions() -> DashboardState {
    let mut second = running_session();
    second.id = "session-2".into();
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.state.sessions.insert(second.id.clone(), second);
    dashboard
}

/// Launch finding B-4: a launch that finishes after the user selected
/// another row must not take the selection back, or the next session
/// command (such as close) acts on a session the user did not choose.
#[test]
fn a_finished_launch_leaves_a_selection_the_user_moved_elsewhere() {
    let mut dashboard = dashboard_with_two_sessions();
    // The launch selected its session when it started.
    dashboard.select_active_session("session-1");
    // While it launches, the user picks the other session.
    dashboard.select_active_session("session-2");
    dashboard.focus_sessions();

    dashboard.finish_new_session("session-1");

    assert_eq!(dashboard.selected_session_id(), Some("session-2"));
    assert_eq!(dashboard.focus(), Focus::Sessions);
}

/// R4-11: a Kimi session made in the wizard took about 30 seconds to launch
/// and then did not open; the selection sat on the first row, which is where
/// the clamp puts it when a refresh does not carry the selected session.
/// Nobody chose that row, so finishing the launch still opens the session.
#[test]
fn a_finished_launch_opens_the_session_a_refresh_displaced() {
    let mut dashboard = dashboard_with_two_sessions();
    dashboard.select_active_session("session-2");
    let mut refreshed = dashboard.state.clone();
    let launching = refreshed
        .sessions
        .remove("session-2")
        .expect("the launching session");
    dashboard.set_state(refreshed.clone());
    assert_eq!(dashboard.selected_session_id(), Some("session-1"));
    refreshed.sessions.insert(launching.id.clone(), launching);
    dashboard.set_state(refreshed);

    dashboard.finish_new_session("session-2");

    assert_eq!(dashboard.selected_session_id(), Some("session-2"));
    assert_eq!(dashboard.focus(), Focus::Prompt);
}

/// R4-11: after a suspend, the dashboard reported "Could not open Session
/// claude-b: Session opening did not respond within 15 seconds" although
/// nothing asked to open it. The pane still named the suspended session, and
/// the end of its lifecycle re-armed an attach to a session with no worker.
#[test]
fn a_suspended_session_is_not_attached_to() {
    let mut dashboard = dashboard_with_two_sessions();
    assert!(dashboard.pane_session_can_attach("session-1"));
    dashboard
        .state
        .sessions
        .get_mut("session-1")
        .expect("session")
        .state = SessionState::Stopped;
    assert!(!dashboard.pane_session_can_attach("session-1"));
}

/// When the selection is still on the launching session, finishing the
/// launch opens it for its first prompt, as before.
#[test]
fn a_finished_launch_opens_the_session_the_user_left_selected() {
    let mut dashboard = dashboard_with_two_sessions();
    dashboard.select_active_session("session-1");

    dashboard.finish_new_session("session-1");

    assert_eq!(dashboard.selected_session_id(), Some("session-1"));
    assert_eq!(dashboard.focus(), Focus::Prompt);
}

#[test]
fn setting_the_current_session_writes_only_the_focused_pane() {
    let mut dashboard = dashboard_with_two_sessions();
    dashboard.set_current_session(Some("session-1"));
    let first = dashboard.focused_pane();
    let second = dashboard
        .split_focused_pane(ratatui::layout::Direction::Horizontal, Some("session-2"))
        .expect("the test conversation area has room for two panes");

    dashboard.set_current_session(Some("session-1"));

    assert_eq!(dashboard.focused_pane(), second);
    assert_eq!(dashboard.pane_session(second), Some("session-1"));
    // A session belongs to one pane, so showing it here took it out of the
    // pane it was in; no other pane's entry is touched.
    assert_eq!(dashboard.pane_session(first), None);
    dashboard.set_current_session(Some("session-2"));
    assert_eq!(dashboard.pane_session(second), Some("session-2"));
    assert_eq!(dashboard.pane_session(first), None);
    dashboard.set_current_session(None);
    assert_eq!(dashboard.pane_session(second), None);
}

#[test]
fn a_split_shows_its_session_in_the_new_pane_and_takes_the_focus() {
    let mut dashboard = dashboard_with_two_sessions();
    dashboard.set_current_session(Some("session-1"));
    let first = dashboard.focused_pane();

    let second = dashboard
        .split_focused_pane(ratatui::layout::Direction::Horizontal, Some("session-2"))
        .expect("the test conversation area has room for two panes");

    assert_ne!(second, first);
    assert_eq!(dashboard.focused_pane(), second);
    assert_eq!(dashboard.current_session_id(), Some("session-2"));
    assert_eq!(dashboard.pane_session(first), Some("session-1"));
    assert_eq!(dashboard.pane_for_session("session-1"), Some(first));
    // Focusing a pane moves the Sessions highlight onto what it shows.
    assert_eq!(dashboard.selected_session_id(), Some("session-1"));
    dashboard.focus_pane(first);
    assert_eq!(dashboard.selected_session_id(), Some("session-1"));
}

#[test]
fn closing_the_browse_pane_is_refused_without_displacing_its_session() {
    let mut dashboard = dashboard_with_two_sessions();
    dashboard.set_current_session(Some("session-1"));
    let only = dashboard.focused_pane();
    assert_eq!(dashboard.close_pane(only), None);
    assert_eq!(dashboard.focused_pane(), only);
    assert_eq!(dashboard.current_session_id(), Some("session-1"));
}

#[test]
fn the_conversation_layout_round_trips_with_its_sessions_and_focus() {
    let mut dashboard = dashboard_with_two_sessions();
    dashboard.set_current_session(Some("session-1"));
    let first = dashboard.focused_pane();
    let second = dashboard
        .split_focused_pane(ratatui::layout::Direction::Vertical, Some("session-2"))
        .expect("the test conversation area has room for two panes");
    dashboard.focus_pane(first);
    let saved = dashboard.conversation_layout_for(mj_core::workspace::DEFAULT_WORKSPACE_ID);
    assert!(saved.validate().is_ok());

    let mut restored = dashboard_with_two_sessions();
    restored.cache_workspace_layout(mj_core::workspace::DEFAULT_WORKSPACE_ID, saved.clone());

    assert_eq!(restored.focused_pane(), first);
    assert_eq!(restored.pane_session(first), Some("session-1"));
    assert_eq!(restored.pane_session(second), Some("session-2"));
    assert_eq!(
        restored.conversation_layout_for(mj_core::workspace::DEFAULT_WORKSPACE_ID),
        saved
    );
}

#[test]
fn a_restored_pane_whose_session_is_gone_opens_empty() {
    let mut dashboard = dashboard_with_two_sessions();
    dashboard.set_current_session(Some("session-1"));
    let first = dashboard.focused_pane();
    let second = dashboard
        .split_focused_pane(ratatui::layout::Direction::Horizontal, Some("session-2"))
        .expect("the test conversation area has room for two panes");
    let saved = dashboard.conversation_layout_for(mj_core::workspace::DEFAULT_WORKSPACE_ID);

    // The second session was closed and removed while this client was away.
    let mut restored = dashboard_with_session(running_session());
    restored.cache_workspace_layout(mj_core::workspace::DEFAULT_WORKSPACE_ID, saved);

    assert_eq!(restored.pane_session(first), Some("session-1"));
    assert_eq!(restored.pane_session(second), None);
}

/// The session row's menu is where a split is created, so both commands have
/// to be in it.
#[test]
fn the_session_menu_offers_pin_controls() {
    let mut dashboard = dashboard_with_two_sessions();
    dashboard.focus_sessions();
    dashboard.begin_session_palette();

    let lines = drawn(&mut dashboard, 120, 60);
    let joined = lines.join("\n");
    assert!(joined.contains("Pin…"), "{joined}");
    assert!(joined.contains("Unpin"), "{joined}");
}

/// The pane commands belong where a conversation is: at the composer, and in
/// the Sessions list beside it.
#[test]
fn the_command_palette_offers_close_pane_at_the_composer() {
    let mut dashboard = dashboard_with_two_sessions();
    dashboard.set_current_session(Some("session-1"));
    dashboard.focus_prompt();
    open_palette(&mut dashboard);

    let joined = drawn(&mut dashboard, 120, 60).join("\n");
    assert!(joined.contains("Close pane"), "{joined}");
    assert!(joined.contains("Focus pane right"), "{joined}");
    assert!(joined.contains("Resize pane left"), "{joined}");
}

/// Opening a session already on screen is a move, not a copy: the same
/// conversation must never be drawn in two panes.
#[test]
fn opening_a_session_shown_elsewhere_is_answered_by_the_pane_that_has_it() {
    let mut dashboard = dashboard_with_two_sessions();
    dashboard.set_current_session(Some("session-1"));
    let first = dashboard.focused_pane();
    let second = dashboard
        .split_focused_pane(ratatui::layout::Direction::Horizontal, Some("session-2"))
        .expect("the test conversation area has room for two panes");
    dashboard.focus_sessions();
    dashboard.select_active_session("session-1");

    let action = dashboard.dispatch_command(CommandId::OpenSession);

    // The controller answers `Open` by moving the focus to the pane that
    // already shows it rather than attaching a second copy.
    assert!(
        matches!(&action, DashboardAction::Open { session_id } if session_id == "session-1"),
        "{action:?}"
    );
    assert_eq!(dashboard.pane_for_session("session-1"), Some(first));
    assert_eq!(dashboard.pane_session(second), Some("session-2"));
}

/// A split that would leave either half too small to use is refused, leaves
/// the layout unchanged, and says why on the notice bar.
#[test]
fn a_split_with_no_room_is_refused_and_changes_nothing() {
    let mut dashboard = dashboard_with_two_sessions();
    dashboard.set_current_session(Some("session-1"));
    // A narrow frame leaves a conversation band too slim for two panes.
    drawn(&mut dashboard, 80, 30);
    let before = dashboard.focused_pane();

    let refused = dashboard.split_focused_pane(ratatui::layout::Direction::Horizontal, None);

    assert!(refused.is_none());
    assert_eq!(dashboard.focused_pane(), before);
    assert_eq!(dashboard.conversation_layout.pane_count(), 1);
    assert_eq!(
        dashboard.notice().as_deref(),
        Some(crate::SPLIT_REFUSED_NOTICE)
    );
}

/// Launch finding D-2: the refusal notice describes a failed split, so a
/// later split that succeeds must take it off the bar.
#[test]
fn a_successful_split_clears_the_earlier_no_room_notice() {
    let mut dashboard = dashboard_with_two_sessions();
    dashboard.set_current_session(Some("session-1"));
    drawn(&mut dashboard, 80, 30);
    assert!(
        dashboard
            .split_focused_pane(ratatui::layout::Direction::Horizontal, None)
            .is_none()
    );
    assert!(dashboard.notice().is_some());

    drawn(&mut dashboard, 200, 60);
    dashboard
        .split_focused_pane(ratatui::layout::Direction::Horizontal, None)
        .expect("a wide frame has room for two panes");

    assert_eq!(dashboard.notice(), None);
}

/// Moving the focus between panes is a layout change the controller has to
/// save, and one that carries the Sessions highlight with it.
#[test]
fn focusing_a_pane_toward_a_direction_reports_the_change_and_moves_the_highlight() {
    let mut dashboard = dashboard_with_two_sessions();
    dashboard.set_current_session(Some("session-1"));
    let first = dashboard.focused_pane();
    dashboard
        .split_focused_pane(ratatui::layout::Direction::Horizontal, Some("session-2"))
        .expect("the test conversation area has room for two panes");

    let action = dashboard.dispatch_command(CommandId::FocusPaneLeft);

    assert_eq!(
        action,
        DashboardAction::ConversationPanesChanged { focus_moved: true }
    );
    assert_eq!(dashboard.focused_pane(), first);
    assert_eq!(dashboard.selected_session_id(), Some("session-1"));
    // There is nothing further left, so the command changes nothing.
    assert_eq!(
        dashboard.dispatch_command(CommandId::FocusPaneLeft),
        DashboardAction::None
    );
}

/// An attach lands in the pane that asked for it, whichever pane has the
/// keyboard by the time it arrives, and a session is never in two panes.
#[test]
fn a_session_installs_into_the_pane_that_asked_and_leaves_any_other() {
    let mut dashboard = dashboard_with_two_sessions();
    dashboard.set_current_session(Some("session-1"));
    let first = dashboard.focused_pane();
    let second = dashboard
        .split_focused_pane(ratatui::layout::Direction::Horizontal, Some("session-2"))
        .expect("the test conversation area has room for two panes");

    // The attach the first pane started arrives while the second has focus.
    dashboard.set_pane_session(first, Some("session-1"));

    assert_eq!(dashboard.focused_pane(), second);
    assert_eq!(dashboard.pane_session(first), Some("session-1"));
    assert_eq!(dashboard.pane_session(second), Some("session-2"));

    // Moving a session into another pane takes it out of the one it was in.
    dashboard.set_pane_session(second, Some("session-1"));
    assert_eq!(dashboard.pane_session(first), None);
    assert_eq!(dashboard.pane_for_session("session-1"), Some(second));
}

/// Closing a pin preserves the independent list cursor.
#[test]
fn closing_a_pane_preserves_the_list_cursor() {
    let mut dashboard = dashboard_with_two_sessions();
    dashboard.set_current_session(Some("session-1"));
    let first = dashboard.focused_pane();
    let second = dashboard
        .split_focused_pane(ratatui::layout::Direction::Horizontal, Some("session-2"))
        .unwrap();
    dashboard.focus_pane(first);
    dashboard.select_active_session("session-1");
    assert_eq!(dashboard.close_pane(first).as_deref(), Some("session-1"));
    assert_eq!(dashboard.focused_pane(), second);
    assert_eq!(dashboard.selected_session_id(), Some("session-1"));
}

/// A stored arrangement that names one session in two panes is not a layout
/// this surface can draw: the conversation would appear twice.
#[test]
fn a_restored_layout_never_shows_one_session_in_two_panes() {
    let mut dashboard = dashboard_with_two_sessions();
    dashboard.set_current_session(Some("session-1"));
    let first = dashboard.focused_pane();
    let second = dashboard
        .split_focused_pane(ratatui::layout::Direction::Horizontal, Some("session-2"))
        .expect("the test conversation area has room for two panes");
    let mut saved = dashboard.conversation_layout_for(mj_core::workspace::DEFAULT_WORKSPACE_ID);
    saved.sessions.insert(second.raw(), "session-1".to_owned());

    let mut restored = dashboard_with_two_sessions();
    restored.cache_workspace_layout(mj_core::workspace::DEFAULT_WORKSPACE_ID, saved);

    assert_eq!(restored.pane_session(first), Some("session-1"));
    assert_eq!(restored.pane_session(second), None);
}

/// The keys herdr uses for panes: `prefix+v` splits beside, `prefix+x`
/// closes. The split itself belongs to the controller, which opens the
/// session in the new pane, so the test does that step the way `mj-cli`
/// does and then closes the pane with its own key.
#[test]
fn the_pane_keys_split_beside_the_conversation_and_close_a_pin() {
    let mut dashboard = dashboard_with_two_sessions();
    dashboard.set_current_session(Some("session-1"));
    dashboard.select_active_session("session-2");
    dashboard.focus_prompt();
    let first = dashboard.focused_pane();
    let action = chord(&mut dashboard, CommandId::OpenSessionSplitRight);
    let DashboardAction::SplitConversation { pane, direction } = action else {
        panic!("expected a targeted split: {action:?}");
    };
    assert_eq!(pane, first);
    assert_eq!(direction, ratatui::layout::Direction::Horizontal);
    let browse = dashboard.split_conversation_pane(pane, direction).unwrap();
    assert_eq!(dashboard.focused_pane(), browse);
    assert_eq!(dashboard.pane_session(browse), None);
    assert_eq!(dashboard.pin_id("session-1"), Some(0));
    dashboard.focus_pane(first);
    assert_eq!(
        chord(&mut dashboard, CommandId::ClosePane),
        DashboardAction::ClosePane { pane: first }
    );
    assert_eq!(dashboard.close_pane(first).as_deref(), Some("session-1"));
    assert_eq!(dashboard.browse_pane(), browse);
    assert_eq!(dashboard.conversation_layout.pane_count(), 1);
}

#[test]
fn a_pane_close_chip_closes_its_own_pane_without_moving_the_keyboard() {
    let mut dashboard = dashboard_with_two_sessions();
    dashboard.config.advanced.symbols = Some(mj_core::config::SymbolSet::Unicode);
    dashboard.set_current_session(Some("session-1"));
    let first = dashboard.focused_pane();
    let second = dashboard
        .split_focused_pane(ratatui::layout::Direction::Horizontal, Some("session-2"))
        .expect("the test conversation area has room for two panes");
    dashboard.focus_prompt();

    let lines = drawn(&mut dashboard, 120, 40);
    let chip = |dashboard: &DashboardState, pane| {
        let (transcript, _) = dashboard.pane_bands(pane).expect("a drawn pane");
        (transcript.right() - 3, transcript.y)
    };
    {
        let (column, row) = chip(&dashboard, first);
        assert_eq!(
            lines[row as usize].chars().nth(column as usize),
            Some('×'),
            "pinned panes draw a close chip on their title row: {lines:#?}"
        );
    }

    let target = chip(&dashboard, first);
    assert_eq!(
        dashboard.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), target)),
        DashboardAction::None
    );
    assert_eq!(
        dashboard.handle_mouse(mouse_at(MouseEventKind::Up(MouseButton::Left), target)),
        DashboardAction::ClosePane { pane: first }
    );
    assert_eq!(dashboard.focused_pane(), second);
    assert_eq!(dashboard.selected_session_id(), Some("session-1"));
}

/// The last-pane key returns the keyboard to the pane it came from, and says
/// so when there is no pane to go back to.
#[test]
fn the_last_pane_key_returns_to_the_pane_the_keyboard_came_from() {
    let mut dashboard = dashboard_with_two_sessions();
    dashboard.set_current_session(Some("session-1"));
    let first = dashboard.focused_pane();
    dashboard.focus_prompt();

    // Nothing has moved yet, so there is nothing to go back to.
    assert_eq!(
        chord(&mut dashboard, CommandId::FocusLastPane),
        DashboardAction::None
    );
    assert_eq!(dashboard.notice().as_deref(), Some("No previous pane"));
    assert_eq!(dashboard.focused_pane(), first);

    let second = dashboard
        .split_focused_pane(ratatui::layout::Direction::Horizontal, Some("session-2"))
        .expect("the test conversation area has room for two panes");
    dashboard.focus_pane(first);
    dashboard.focus_pane(second);
    dashboard.focus_prompt();

    assert_eq!(
        chord(&mut dashboard, CommandId::FocusLastPane),
        DashboardAction::ConversationPanesChanged { focus_moved: true }
    );
    assert_eq!(dashboard.focused_pane(), first);

    // It is a toggle: running it again goes back to where it just came from.
    assert_eq!(
        chord(&mut dashboard, CommandId::FocusLastPane),
        DashboardAction::ConversationPanesChanged { focus_moved: true }
    );
    assert_eq!(dashboard.focused_pane(), second);
}

/// Closing a pane forgets it as the place to go back to, so the key never
/// moves the keyboard into a pane that is gone.
#[test]
fn closing_a_pane_leaves_no_previous_pane_to_return_to() {
    let mut dashboard = dashboard_with_two_sessions();
    dashboard.set_current_session(Some("session-1"));
    let first = dashboard.focused_pane();
    let second = dashboard
        .split_focused_pane(ratatui::layout::Direction::Horizontal, Some("session-2"))
        .expect("the test conversation area has room for two panes");
    dashboard.focus_pane(first);
    dashboard.focus_pane(second);
    dashboard.focus_prompt();

    dashboard.close_pane(first);

    assert_eq!(
        chord(&mut dashboard, CommandId::FocusLastPane),
        DashboardAction::None
    );
    assert_eq!(dashboard.notice().as_deref(), Some("No previous pane"));
    assert_eq!(dashboard.focused_pane(), second);
}

/// Zoom fills the conversation band with the focused pane: the other panes
/// keep their place in the arrangement but are neither drawn nor pointed at,
/// and the zoom toggles back off.
#[test]
fn zoom_fills_the_band_with_the_focused_pane_and_toggles_back() {
    let mut dashboard = dashboard_with_two_sessions();
    dashboard.set_current_session(Some("session-1"));
    let first = dashboard.focused_pane();
    let second = dashboard
        .split_focused_pane(ratatui::layout::Direction::Horizontal, Some("session-2"))
        .expect("the test conversation area has room for two panes");
    dashboard.focus_prompt();
    drawn(&mut dashboard, 120, 40);
    let band = dashboard.conversation_area.expect("the conversation band");

    assert_eq!(
        chord(&mut dashboard, CommandId::ZoomPane),
        DashboardAction::ConversationPanesChanged { focus_moved: false }
    );
    assert!(dashboard.conversation_zoomed());
    drawn(&mut dashboard, 120, 40);

    let panes = dashboard.conversation_panes(band);
    assert_eq!(panes.len(), 1);
    assert_eq!(panes[0].id, second);
    assert_eq!(panes[0].rect, band);
    // Hit-testing agrees with what was drawn: the hidden pane answers for
    // nothing, and the zoomed pane answers for the whole band.
    let (transcript, _) = dashboard.pane_bands(second).expect("the zoomed pane drew");
    assert_eq!(transcript.width, band.width);
    assert!(dashboard.pane_bands(first).is_none());
    assert_eq!(
        dashboard.chat_region_contains(band.x + 1, band.y + 1),
        Some(second)
    );

    // The arrangement underneath is untouched, so unzooming puts both back.
    chord(&mut dashboard, CommandId::ZoomPane);
    assert!(!dashboard.conversation_zoomed());
    drawn(&mut dashboard, 120, 40);
    assert_eq!(dashboard.conversation_panes(band).len(), 2);
    assert!(dashboard.pane_bands(first).is_some());
}

/// Directional focus works against the arrangement the zoom hides, and moving
/// the keyboard keeps the zoom: the new pane fills the band in its turn.
#[test]
fn focusing_another_pane_while_zoomed_keeps_the_zoom() {
    let mut dashboard = dashboard_with_two_sessions();
    dashboard.set_current_session(Some("session-1"));
    let first = dashboard.focused_pane();
    let second = dashboard
        .split_focused_pane(ratatui::layout::Direction::Horizontal, Some("session-2"))
        .expect("the test conversation area has room for two panes");
    dashboard.focus_prompt();
    drawn(&mut dashboard, 120, 40);
    chord(&mut dashboard, CommandId::ZoomPane);

    assert_eq!(
        chord(&mut dashboard, CommandId::FocusPaneLeft),
        DashboardAction::ConversationPanesChanged { focus_moved: true }
    );

    assert_eq!(dashboard.focused_pane(), first);
    assert!(dashboard.conversation_zoomed());
    let band = dashboard.conversation_area.expect("the conversation band");
    let panes = dashboard.conversation_panes(band);
    assert_eq!(panes.len(), 1);
    assert_eq!(panes[0].id, first);
    assert_ne!(second, first);
}

/// Splitting and closing both change which panes there are, so each one ends
/// the zoom rather than leaving panes hidden behind it.
#[test]
fn a_split_or_a_close_ends_the_zoom() {
    let mut dashboard = dashboard_with_two_sessions();
    dashboard.set_current_session(Some("session-1"));
    let first = dashboard.focused_pane();
    dashboard
        .split_focused_pane(ratatui::layout::Direction::Horizontal, Some("session-2"))
        .expect("the test conversation area has room for two panes");
    dashboard.focus_prompt();
    drawn(&mut dashboard, 120, 40);

    chord(&mut dashboard, CommandId::ZoomPane);
    assert!(dashboard.conversation_zoomed());
    dashboard
        .split_focused_pane(ratatui::layout::Direction::Vertical, None)
        .expect("the test conversation area has room for a third pane");
    assert!(!dashboard.conversation_zoomed());

    chord(&mut dashboard, CommandId::ZoomPane);
    assert!(dashboard.conversation_zoomed());
    dashboard.close_pane(first);
    assert!(!dashboard.conversation_zoomed());
}

/// A lone pane already fills the band, so the command says so instead of
/// changing nothing silently.
#[test]
fn zooming_a_lone_pane_says_there_is_nothing_to_zoom() {
    let mut dashboard = dashboard_with_two_sessions();
    dashboard.set_current_session(Some("session-1"));
    dashboard.focus_prompt();

    assert_eq!(
        chord(&mut dashboard, CommandId::ZoomPane),
        DashboardAction::None
    );

    assert!(!dashboard.conversation_zoomed());
    assert_eq!(
        dashboard.notice().as_deref(),
        Some("Only one pane; nothing to zoom")
    );
}

/// The zoomed pane carries a `Z` chip left of its close chip, and clicking it
/// puts the other panes back.
#[test]
fn clicking_the_zoom_chip_unzooms() {
    let mut dashboard = dashboard_with_two_sessions();
    dashboard.config.advanced.symbols = Some(mj_core::config::SymbolSet::Unicode);
    dashboard.set_current_session(Some("session-1"));
    dashboard
        .split_focused_pane(ratatui::layout::Direction::Horizontal, Some("session-2"))
        .expect("the test conversation area has room for two panes");
    dashboard.focus_prompt();
    drawn(&mut dashboard, 120, 40);
    chord(&mut dashboard, CommandId::ZoomPane);

    let lines = drawn(&mut dashboard, 120, 40);
    let (transcript, _) = dashboard
        .pane_bands(dashboard.focused_pane())
        .expect("the zoomed pane drew");
    let chip = (transcript.right() - 6, transcript.y);
    assert_eq!(
        lines[chip.1 as usize].chars().nth(chip.0 as usize),
        Some('Z'),
        "the zoomed pane draws a Z chip left of its close chip: {lines:#?}"
    );

    assert_eq!(
        dashboard.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), chip)),
        DashboardAction::None
    );
    assert_eq!(
        dashboard.handle_mouse(mouse_at(MouseEventKind::Up(MouseButton::Left), chip)),
        DashboardAction::ConversationPanesChanged { focus_moved: false }
    );
    assert!(!dashboard.conversation_zoomed());
}

/// The pane with the keyboard draws its transcript border in the focused
/// style, and its neighbour does not, so the focused pane is visible without
/// reading the composer.
#[test]
fn the_focused_pane_draws_a_distinct_transcript_border() {
    let mut dashboard = dashboard_with_two_sessions();
    dashboard.set_current_session(Some("session-1"));
    let first = dashboard.focused_pane();
    let second = dashboard
        .split_focused_pane(ratatui::layout::Direction::Horizontal, Some("session-2"))
        .expect("the test conversation area has room for two panes");
    assert_eq!(dashboard.focused_pane(), second);

    let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("test terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw two panes");
    let corner = |dashboard: &DashboardState, pane| {
        let (transcript, _) = dashboard.pane_bands(pane).expect("a drawn pane");
        (transcript.x, transcript.y)
    };
    let buffer = terminal.backend().buffer();
    assert_eq!(
        Some(buffer[corner(&dashboard, second)].fg),
        mj_chat::theme::border(true).fg,
        "the focused pane's transcript border uses the focused style"
    );
    assert_eq!(
        buffer[corner(&dashboard, first)].fg,
        mj_chat::theme::palette().border,
        "an unfocused pane keeps the resting border"
    );
}

/// With one pane there is nothing to tell apart, so the transcript border
/// stays at rest and the single-pane surface draws as it always has.
#[test]
fn the_lone_pane_draws_a_resting_transcript_border() {
    let mut dashboard = dashboard_with_two_sessions();
    dashboard.set_current_session(Some("session-1"));
    let pane = dashboard.focused_pane();

    let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("test terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw one pane");
    let (transcript, _) = dashboard.pane_bands(pane).expect("a drawn pane");
    assert_eq!(
        terminal.backend().buffer()[(transcript.x, transcript.y)].fg,
        mj_chat::theme::palette().border,
        "a lone pane draws the resting border whether or not it has the keyboard"
    );
}

/// Closing a pane that does not hold the keyboard removes it and reports what
/// it showed, leaving the focus and the highlight alone.
#[test]
fn closing_an_unfocused_pane_leaves_the_keyboard_where_it_was() {
    let mut dashboard = dashboard_with_two_sessions();
    dashboard.set_current_session(Some("session-1"));
    let first = dashboard.focused_pane();
    let second = dashboard
        .split_focused_pane(ratatui::layout::Direction::Horizontal, Some("session-2"))
        .expect("the test conversation area has room for two panes");

    let closed = dashboard.close_pane(first);

    assert_eq!(closed.as_deref(), Some("session-1"));
    assert_eq!(dashboard.conversation_layout.pane_count(), 1);
    assert_eq!(dashboard.focused_pane(), second);
    assert_eq!(dashboard.selected_session_id(), Some("session-1"));
    assert_eq!(dashboard.pane_session(second), Some("session-2"));
}

/// `prefix+minus` stacks instead of sitting beside, and the focus keys move
/// between the two panes from the composer.
#[test]
fn the_stacked_split_key_and_the_focus_keys_answer_from_the_composer() {
    let mut dashboard = dashboard_with_two_sessions();
    dashboard.set_current_session(Some("session-1"));
    dashboard.select_active_session("session-2");
    dashboard.focus_prompt();
    let first = dashboard.focused_pane();

    let action = chord(&mut dashboard, CommandId::OpenSessionSplitBelow);

    let DashboardAction::SplitConversation { direction, .. } = action else {
        panic!("the stacked split key should ask for a split: {action:?}");
    };
    assert_eq!(direction, ratatui::layout::Direction::Vertical);
    dashboard
        .split_focused_pane(direction, Some("session-2"))
        .expect("the test conversation area has room for two panes");

    assert_eq!(
        chord(&mut dashboard, CommandId::FocusPaneUp),
        DashboardAction::ConversationPanesChanged { focus_moved: true }
    );
    assert_eq!(dashboard.focused_pane(), first);
    assert_eq!(dashboard.selected_session_id(), Some("session-2"));
}

/// A split key pressed with nothing selected still splits; the new pane is
/// simply empty. The controller answers `SplitPane` by splitting and leaving
/// the leaf without a session.
#[test]
fn a_split_key_with_no_selection_asks_for_an_empty_pane() {
    let mut dashboard = dashboard_with_two_sessions();
    dashboard.state.sessions.clear();
    dashboard.clamp_selections();
    dashboard.focus_prompt();

    assert_eq!(dashboard.selected_session_id(), None);
    assert_eq!(
        chord(&mut dashboard, CommandId::OpenSessionSplitRight),
        DashboardAction::SplitConversation {
            pane: dashboard.focused_pane(),
            direction: ratatui::layout::Direction::Horizontal
        }
    );
}

/// A restored arrangement comes back with the keyboard in the pane it named
/// and the Sessions highlight on that pane's session. Without the highlight
/// following, the controller's "open what is selected" step would pull the
/// first row's conversation into the restored focus pane.
#[test]
fn a_restored_arrangement_keeps_its_focus_and_its_highlight() {
    let mut dashboard = dashboard_with_two_sessions();
    dashboard.set_current_session(Some("session-1"));
    let second = dashboard
        .split_focused_pane(ratatui::layout::Direction::Horizontal, Some("session-2"))
        .expect("the test conversation area has room for two panes");
    let saved = dashboard.conversation_layout_for(mj_core::workspace::DEFAULT_WORKSPACE_ID);
    assert_eq!(saved.focus, second.raw());

    let mut restored = dashboard_with_two_sessions();
    restored.cache_workspace_layout(mj_core::workspace::DEFAULT_WORKSPACE_ID, saved);

    assert_eq!(restored.focused_pane(), second);
    assert_eq!(restored.current_session_id(), Some("session-2"));
    assert_eq!(restored.selected_session_id(), Some("session-1"));
}

/// Three live sessions in the default workspace and one in `other`: `asks`
/// is waiting on a question, `done` has an unread answer, `quiet` is idle and
/// read, and `remote` (in `other`) is also waiting on a question.
fn dashboard_with_attention_mix() -> DashboardState {
    let mut sessions = BTreeMap::new();
    for (id, workspace, created) in [
        ("quiet", "default", "2026-08-01T00:00:00Z"),
        ("asks", "default", "2026-08-02T00:00:00Z"),
        ("done", "default", "2026-08-03T00:00:00Z"),
        ("remote", "other", "2026-08-04T00:00:00Z"),
    ] {
        let mut session = running_session();
        session.id = id.into();
        session.workspace_id = workspace.into();
        session.created_at = created.into();
        session.project_directory = Some(format!("/projects/{id}").into());
        sessions.insert(session.id.clone(), session);
    }
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
    dashboard.set_workspace_names(BTreeMap::from([
        ("default".into(), "Default".into()),
        ("other".into(), "Other".into()),
    ]));
    for id in ["asks", "remote"] {
        dashboard
            .session_details
            .get_mut(id)
            .unwrap()
            .pending_elicitations = vec![question(id)];
    }
    dashboard
        .session_details
        .get_mut("done")
        .unwrap()
        .unread_agent_messages = 1;
    dashboard
}

#[test]
fn attention_levels_rank_a_question_above_unread_above_idle() {
    let dashboard = dashboard_with_attention_mix();
    assert_eq!(dashboard.attention_level("asks"), AttentionLevel::Waiting);
    assert_eq!(dashboard.attention_level("done"), AttentionLevel::Unread);
    assert_eq!(dashboard.attention_level("quiet"), AttentionLevel::Idle);
    assert_eq!(
        dashboard.attention_level("missing"),
        AttentionLevel::Inactive
    );
    let queue = dashboard
        .attention_queue()
        .into_iter()
        .map(|entry| entry.session_id)
        .collect::<Vec<_>>();
    // Both questions lead; the unread answer follows; the idle session is
    // not in the queue at all.
    assert_eq!(queue.len(), 3);
    assert!(queue[..2].contains(&"asks".to_owned()));
    assert!(queue[..2].contains(&"remote".to_owned()));
    assert_eq!(queue[2], "done");
}

#[test]
fn a_failure_outranks_a_question_and_an_unreachable_worker_sits_between_them() {
    let mut dashboard = dashboard_with_attention_mix();
    // "asks" already has a pending question. A review that failed on the same
    // session must win, so the row, the queue, and the badge all say failure.
    dashboard.set_session_reviews([mj_client::review::RuntimeReviewView {
        session_id: "asks".into(),
        tier: mj_core::review::lanes::ReviewTier::Quick,
        phase: mj_core::review::driver::TurnReviewPhase::Verdict(
            mj_core::review::verdict::ReviewVerdict::Failed {
                reason: "the reviewer never answered".into(),
            },
        ),
        roles: Vec::new(),
        status: "the review failed".into(),
        verdict: None,
    }]);
    dashboard.set_session_connectivity("remote", false);

    assert_eq!(dashboard.attention_level("asks"), AttentionLevel::Failed);
    assert_eq!(
        dashboard.attention_level("remote"),
        AttentionLevel::Unreachable,
        "an unreachable worker is its own level, above a question"
    );
    assert!(AttentionLevel::Failed > AttentionLevel::Unreachable);
    assert!(AttentionLevel::Unreachable > AttentionLevel::Waiting);

    let queue = dashboard
        .attention_queue()
        .into_iter()
        .map(|entry| entry.session_id)
        .collect::<Vec<_>>();
    assert_eq!(queue, vec!["asks", "remote", "done"]);
}

#[test]
fn a_failed_stop_is_a_failure_for_the_queue_as_well_as_the_row() {
    let mut dashboard = dashboard_with_attention_mix();
    let session = dashboard.state.sessions.get_mut("quiet").unwrap();
    session.state = SessionState::Closing;
    session.last_error = Some("the worker would not stop".into());

    assert_eq!(dashboard.attention_level("quiet"), AttentionLevel::Failed);
    assert_eq!(
        dashboard
            .attention_queue()
            .first()
            .map(|entry| &*entry.session_id),
        Some("quiet")
    );
}

#[test]
fn a_badge_counts_only_sessions_at_its_displayed_attention_level() {
    let mut dashboard = dashboard_with_attention_mix();
    dashboard.set_active_workspace(Some("default".into()));
    // Default holds one question and one unread answer.
    assert_eq!(
        dashboard.workspace_attention_summary("default"),
        Some((AttentionLevel::Waiting, 1))
    );

    dashboard.set_session_connectivity("done", false);
    assert_eq!(
        dashboard.workspace_attention_summary("default"),
        Some((AttentionLevel::Unreachable, 1))
    );

    let session = dashboard.state.sessions.get_mut("quiet").unwrap();
    session.state = SessionState::Error;
    assert_eq!(
        dashboard.workspace_attention_summary("default"),
        Some((AttentionLevel::Failed, 1))
    );

    let badge =
        crate::render::sessions::attention_badge(dashboard.workspace_attention_summary("default"))
            .expect("a workspace with three flagged sessions carries a badge");
    assert_eq!(badge.content, " \u{d7}1");
    assert_eq!(
        badge.style.fg,
        Some(mj_chat::theme::palette().session_error),
        "a failure badge is red, not the attention colour"
    );

    let quiet = dashboard_with_session(running_session());
    assert_eq!(quiet.workspace_attention_summary("default"), None);
    assert!(crate::render::sessions::attention_badge(None).is_none());
}

#[test]
fn viewing_one_failure_reveals_the_five_unread_sessions_without_relabeling_them() {
    let mut failed = running_session();
    failed.id = "failed".into();
    failed.state = SessionState::Error;
    failed.last_error = Some("worker stopped unexpectedly".into());
    let mut dashboard = dashboard_with_session(failed);
    let mut state = dashboard.state.clone();
    for index in 0..5 {
        let mut session = running_session();
        session.id = format!("done-{index}");
        state.sessions.insert(session.id.clone(), session);
    }
    dashboard.set_state(state);
    for index in 0..5 {
        let detail = dashboard
            .session_details
            .get_mut(&format!("done-{index}"))
            .unwrap();
        detail.agent_message_latest_content_ordinals = vec![1];
        detail.unread_agent_messages = 1;
    }
    dashboard.select_active_session("failed");
    assert_eq!(
        dashboard.workspace_attention_summary("default"),
        Some((AttentionLevel::Failed, 1))
    );
    drawn(&mut dashboard, 40, 10);
    dashboard.acknowledge_render();
    assert_eq!(
        dashboard.workspace_attention_summary("default"),
        Some((AttentionLevel::Failed, 1)),
        "a terminal-too-small warning does not show the failure"
    );

    let screen = drawn(&mut dashboard, 140, 40).join("\n");
    assert!(screen.contains("worker stopped unexpectedly"), "{screen}");
    dashboard.acknowledge_render();
    assert_eq!(
        dashboard.workspace_attention_summary("default"),
        Some((AttentionLevel::Unread, 5))
    );
    assert_eq!(
        dashboard.attention_badge_summary(),
        Some((AttentionLevel::Unread, 5))
    );
    assert_eq!(
        dashboard.attention_level("failed"),
        AttentionLevel::Failed,
        "reading a failure does not fix the failed session"
    );

    dashboard.set_state(dashboard.state.clone());
    assert_eq!(
        dashboard.workspace_attention_summary("default"),
        Some((AttentionLevel::Unread, 5)),
        "refreshing unchanged state must not resurrect the notice"
    );
    let mut state = dashboard.state.clone();
    state.sessions.get_mut("failed").unwrap().last_error = Some("a different failure".into());
    dashboard.set_state(state);
    assert_eq!(
        dashboard.workspace_attention_summary("default"),
        Some((AttentionLevel::Failed, 1)),
        "a new error needs attention again"
    );
    drawn(&mut dashboard, 140, 40);
    dashboard.acknowledge_render();
    let mut state = dashboard.state.clone();
    state.sessions.get_mut("failed").unwrap().state = SessionState::Running;
    dashboard.set_state(state.clone());
    state.sessions.get_mut("failed").unwrap().state = SessionState::Error;
    dashboard.set_state(state);
    assert_eq!(
        dashboard.workspace_attention_summary("default"),
        Some((AttentionLevel::Failed, 1)),
        "a later failure after recovery is a new episode even with the same text"
    );
}

#[test]
fn a_session_without_a_row_is_not_counted_by_the_badge_or_the_queue() {
    let mut dashboard = dashboard_with_attention_mix();
    dashboard.set_active_workspace(Some("default".into()));
    assert_eq!(
        dashboard.workspace_attention_summary("default"),
        Some((AttentionLevel::Waiting, 1))
    );

    // A data-loss session is a terminal failure the Sessions pane never lists;
    // the tab badge and the attention queue must not point at it either.
    for state in [SessionState::DestroyedWithDataLoss, SessionState::Lost] {
        dashboard.state.sessions.get_mut("quiet").unwrap().state = state;
        assert_eq!(dashboard.attention_level("quiet"), AttentionLevel::Failed);
        assert!(
            !dashboard
                .ordered_sessions()
                .iter()
                .any(|session| session.id == "quiet"),
            "{state:?} has no row in the pane"
        );
        assert_eq!(
            dashboard.workspace_attention_summary("default"),
            Some((AttentionLevel::Waiting, 1)),
            "{state:?} must not turn the badge red or raise its count"
        );
        assert!(
            !dashboard
                .attention_queue()
                .iter()
                .any(|entry| entry.session_id == "quiet"),
            "{state:?} must not be in the attention queue"
        );
    }

    // An error state still has a row, so it still counts.
    dashboard.state.sessions.get_mut("quiet").unwrap().state = SessionState::Error;
    assert_eq!(
        dashboard.workspace_attention_summary("default"),
        Some((AttentionLevel::Failed, 1))
    );
}

#[test]
fn marking_all_read_clears_the_unread_badge_but_leaves_a_question_flagged() {
    let mut dashboard = dashboard_with_attention_mix();
    dashboard.set_active_workspace(Some("default".into()));
    // Mark all read advances the read marker to the materialized transcript.
    dashboard
        .session_details
        .get_mut("done")
        .unwrap()
        .materialized_applied_event_ordinal = Some(4);
    assert_eq!(
        dashboard.sessions_attention_summary(),
        Some((AttentionLevel::Waiting, 1))
    );

    chord(&mut dashboard, CommandId::MarkAllRead);
    assert_eq!(
        dashboard.attention_level("asks"),
        AttentionLevel::Waiting,
        "a pending question survives mark all read"
    );
    assert_eq!(
        dashboard.sessions_attention_summary(),
        Some((AttentionLevel::Waiting, 1))
    );
}

#[test]
fn next_attention_opens_the_waiting_session_and_wraps_through_the_queue() {
    let mut dashboard = dashboard_with_attention_mix();
    dashboard.set_active_workspace(Some("default".into()));
    // Make the local question the newest so it leads the queue.
    dashboard
        .session_details
        .get_mut("asks")
        .unwrap()
        .last_activity_at_ms = Some(20);
    dashboard
        .session_details
        .get_mut("remote")
        .unwrap()
        .last_activity_at_ms = Some(10);
    dashboard.select_active_session("quiet");

    assert_eq!(
        chord(&mut dashboard, CommandId::NextAttention),
        DashboardAction::Open {
            session_id: "asks".into()
        }
    );
    assert_eq!(dashboard.selected_session_id(), Some("asks"));
    assert!(dashboard.prompt_has_focus());

    // The next entry lives in another workspace: the dashboard records it as
    // that workspace's selection and asks the host to switch tabs.
    assert_eq!(
        chord(&mut dashboard, CommandId::NextAttention),
        DashboardAction::SelectWorkspace {
            workspace_id: "other".into()
        }
    );
    dashboard.set_active_workspace(Some("other".into()));
    assert_eq!(dashboard.selected_session_id(), Some("remote"));

    // From the last entry, previous walks back and next wraps to the front.
    assert_eq!(
        chord(&mut dashboard, CommandId::PreviousAttention),
        DashboardAction::SelectWorkspace {
            workspace_id: "default".into()
        }
    );
    dashboard.set_active_workspace(Some("default".into()));
    assert_eq!(dashboard.selected_session_id(), Some("asks"));
    dashboard.select_active_session("done");
    assert_eq!(
        chord(&mut dashboard, CommandId::NextAttention),
        DashboardAction::Open {
            session_id: "asks".into()
        }
    );
}

#[test]
fn next_attention_reports_an_empty_queue_and_unfolds_a_folded_project() {
    let mut dashboard = dashboard_with_attention_mix();
    dashboard.set_active_workspace(Some("default".into()));
    let key = dashboard
        .project_source(&dashboard.state.sessions["asks"])
        .key;
    dashboard.toggle_project(&key);
    assert!(dashboard.collapsed_project_keys.contains(&key));
    dashboard
        .session_details
        .get_mut("asks")
        .unwrap()
        .last_activity_at_ms = Some(20);
    // From a session outside the queue, the walk starts at the front.
    dashboard.select_active_session("quiet");
    let action = chord(&mut dashboard, CommandId::NextAttention);
    assert_eq!(
        action,
        DashboardAction::Open {
            session_id: "asks".into()
        }
    );
    assert!(!dashboard.collapsed_project_keys.contains(&key));

    for id in ["asks", "remote"] {
        dashboard
            .session_details
            .get_mut(id)
            .unwrap()
            .pending_elicitations
            .clear();
    }
    dashboard
        .session_details
        .get_mut("done")
        .unwrap()
        .unread_agent_messages = 0;
    assert_eq!(
        chord(&mut dashboard, CommandId::NextAttention),
        DashboardAction::None
    );
    assert_eq!(
        dashboard.notices.current().as_deref(),
        Some("Nothing is waiting for you.")
    );
}

#[test]
fn the_footer_names_the_next_key_only_while_something_waits() {
    let mut dashboard = dashboard_with_attention_mix();
    dashboard.set_active_workspace(Some("default".into()));
    // Wide enough for the whole chord list: a narrow row drops the hint for
    // want of room, which says nothing about whether anything is waiting.
    let lines = drawn(&mut dashboard, 200, 40);
    let footer = lines.last().unwrap();
    // The hint carries the same badge as the tabs: the most urgent glyph and
    // how many sessions need that kind of attention.
    assert!(footer.contains("o next (!2)"), "{footer}");

    let mut quiet = dashboard_with_session(running_session());
    let lines = drawn(&mut quiet, 200, 40);
    assert!(
        !lines.last().unwrap().contains("next ("),
        "{}",
        lines.last().unwrap()
    );
}

#[test]
fn workspace_tabs_and_folded_headings_carry_attention_badges() {
    let mut dashboard = dashboard_with_attention_mix();
    dashboard.set_active_workspace(Some("default".into()));
    let lines = drawn(&mut dashboard, 120, 40);
    let tabs = lines
        .iter()
        .find(|line| line.contains("Default") && line.contains("Other"))
        .expect("workspace tab row");
    // A tab counts only its most urgent kind of attention: the
    // default workspace holds one question and one unread answer.
    assert!(tabs.contains("Default !1"), "{tabs}");
    assert!(tabs.contains("Other !1"), "{tabs}");

    // Folding the project that holds the unread session puts its count on
    // the heading; an unfolded project shows the rows instead.
    let key = dashboard
        .project_source(&dashboard.state.sessions["done"])
        .key;
    dashboard.toggle_project(&key);
    let lines = drawn(&mut dashboard, 120, 40);
    let heading = lines
        .iter()
        .find(|line| line.contains("done ✓1"))
        .unwrap_or_else(|| panic!("folded heading with badge: {lines:#?}"));
    assert!(heading.contains("done ✓1"));
}

#[test]
fn priority_order_lists_waiting_first_without_project_headings() {
    let mut dashboard = dashboard_with_attention_mix();
    dashboard.set_active_workspace(Some("default".into()));
    let mut config = dashboard.config.clone();
    config.advanced.session_order = mj_core::config::SessionOrder::Priority;
    dashboard.set_config(config);

    let ids = dashboard
        .ordered_sessions()
        .into_iter()
        .map(|session| session.id.clone())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["asks", "done", "quiet"]);
    assert!(
        dashboard
            .sessions_rows()
            .iter()
            .all(|row| matches!(row, SessionsRow::Session { .. })),
        "priority order has no project headings"
    );
    dashboard.focus_sessions();
    dashboard.handle_key(key(KeyCode::Char('1')));
    assert!(dashboard.collapsed_project_keys.is_empty());
    assert!(
        dashboard
            .notices
            .current()
            .as_deref()
            .is_some_and(|notice| notice.contains("priority order"))
    );

    // Answering the question drops the session below the unread one.
    dashboard
        .session_details
        .get_mut("asks")
        .unwrap()
        .pending_elicitations
        .clear();
    let ids = dashboard
        .ordered_sessions()
        .into_iter()
        .map(|session| session.id.clone())
        .collect::<Vec<_>>();
    assert_eq!(ids[0], "done");
}

#[test]
fn slash_searches_sessions_by_name_and_esc_clears_the_filter() {
    let mut dashboard = dashboard_with_attention_mix();
    dashboard.set_active_workspace(Some("default".into()));
    dashboard.focus_sessions();
    let ids = |dashboard: &DashboardState| {
        dashboard
            .ordered_sessions()
            .into_iter()
            .map(|session| session.id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&dashboard), ["asks", "done", "quiet"]);

    dashboard.handle_key(key(KeyCode::Char('/')));
    for character in "qui".chars() {
        dashboard.handle_key(key(KeyCode::Char(character)));
    }
    assert_eq!(ids(&dashboard), ["quiet"]);
    assert_eq!(dashboard.selected_session_id(), Some("quiet"));
    let lines = drawn(&mut dashboard, 120, 40);
    assert!(
        lines.iter().any(|line| line.contains("Sessions · /qui")),
        "{lines:#?}"
    );
    // At this width the count has no room, and the pane keeps its own name
    // rather than shortening to `S · ` to make the number fit.
    assert!(
        !lines.iter().any(|line| line.contains("hidden")),
        "{lines:#?}"
    );

    // Given the room, the title says how many rows the filter holds back, so a
    // shortened list never reads as the whole truth.
    let wide = drawn(&mut dashboard, 240, 40);
    assert!(
        wide.iter()
            .any(|line| line.contains("Sessions · /qui · 2 hidden")),
        "{wide:#?}"
    );

    // Enter keeps the filter and returns the letters to the pane; `j` moves
    // again instead of typing.
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Char('j')));
    assert_eq!(ids(&dashboard), ["quiet"]);
    assert_eq!(dashboard.selected_session_id(), Some("quiet"));

    // A query nothing matches says so instead of showing an empty pane.
    dashboard.handle_key(key(KeyCode::Char('/')));
    for character in "zzz".chars() {
        dashboard.handle_key(key(KeyCode::Char(character)));
    }
    assert!(ids(&dashboard).is_empty());
    let lines = drawn(&mut dashboard, 120, 40);
    assert!(
        lines.iter().any(|line| line.contains("No sessions match")),
        "{lines:#?}"
    );

    // Esc clears the text, and Esc again drops the filter.
    dashboard.handle_key(key(KeyCode::Esc));
    assert_eq!(ids(&dashboard), ["asks", "done", "quiet"]);
    dashboard.handle_key(key(KeyCode::Esc));
    assert!(dashboard.sessions_filter.is_none());
}

#[test]
fn state_letters_narrow_the_sessions_pane_and_a_shows_all() {
    let mut dashboard = dashboard_with_attention_mix();
    dashboard.set_active_workspace(Some("default".into()));
    dashboard.focus_sessions();
    let ids = |dashboard: &DashboardState| {
        dashboard
            .ordered_sessions()
            .into_iter()
            .map(|session| session.id.clone())
            .collect::<Vec<_>>()
    };
    dashboard.handle_key(key(KeyCode::Char('b')));
    assert_eq!(ids(&dashboard), ["asks"]);
    dashboard.handle_key(key(KeyCode::Char('d')));
    assert_eq!(ids(&dashboard), ["done"]);
    dashboard.handle_key(key(KeyCode::Char('i')));
    assert_eq!(ids(&dashboard), ["quiet"]);
    let lines = drawn(&mut dashboard, 120, 40);
    assert!(
        lines.iter().any(|line| line.contains("Sessions · idle")),
        "{lines:#?}"
    );
    dashboard.handle_key(key(KeyCode::Char('a')));
    assert_eq!(ids(&dashboard), ["asks", "done", "quiet"]);
    assert!(dashboard.sessions_filter.is_none());

    // The state filter and the text filter compose.
    dashboard.handle_key(key(KeyCode::Char('b')));
    dashboard.handle_key(key(KeyCode::Char('/')));
    dashboard.handle_key(key(KeyCode::Char('q')));
    assert!(ids(&dashboard).is_empty());
    dashboard.handle_key(key(KeyCode::Backspace));
    dashboard.handle_key(key(KeyCode::Char('a')));
    assert_eq!(
        ids(&dashboard),
        ["asks"],
        "typing `a` edits the query while editing"
    );

    // Jumping to a session the filter hides drops the filter: with both
    // questions answered, `done` (unread, hidden by the blocked filter) is
    // the only entry left in the queue.
    dashboard.handle_key(key(KeyCode::Enter));
    for id in ["asks", "remote"] {
        dashboard
            .session_details
            .get_mut(id)
            .unwrap()
            .pending_elicitations
            .clear();
    }
    assert_eq!(
        chord(&mut dashboard, CommandId::NextAttention),
        DashboardAction::Open {
            session_id: "done".into()
        }
    );
    assert!(dashboard.sessions_filter.is_none());
}

/// Two running sessions, `alpha` open in the conversation pane and selected,
/// and a reply that has landed for `beta` while it was off screen. The reply
/// arrives the way the host delivers one: a materialized projection whose
/// agent message sits past the read frontier the pane left behind.
fn dashboard_with_an_unread_reply_off_screen() -> DashboardState {
    let mut alpha = running_session();
    alpha.id = "alpha".into();
    alpha.session_title_override = Some("alpha".into());
    alpha.created_at = "2026-08-01T00:00:00Z".into();
    let mut beta = running_session();
    beta.id = "beta".into();
    beta.session_title_override = Some("beta".into());
    beta.created_at = "2026-08-02T00:00:00Z".into();
    // The pane read `beta` through its own prompt before it was left for
    // `alpha`; the answer that follows is the unread one.
    beta.viewed_through_event_ordinal = 3;
    let mut dashboard = dashboard_with_session(alpha);
    dashboard.state.sessions.insert("beta".into(), beta);
    let state = dashboard.state.clone();
    dashboard.set_state(state);
    dashboard.select_active_session("beta");
    dashboard.set_current_session(Some("beta"));
    dashboard.select_active_session("alpha");
    dashboard.set_current_session(Some("alpha"));
    let mut reply = materialized_session_for(
        "beta",
        vec![
            transcript_item(
                3,
                TranscriptBody::User {
                    content: vec![serde_json::json!({"type": "text", "text": "beta prompt"})],
                },
            ),
            agent_message(4, "reliability reply: beta prompt"),
        ],
    );
    reply.execution = MaterializedExecutionState::Idle;
    dashboard.apply_materialized_session(&reply);
    dashboard
}

/// The `d` filter has to keep the row it found, and the row it found is the one
/// drawn with the done glyph.
///
/// The Sessions selection is what the host opens, and opening a conversation
/// marks its answer read. A filter that moved the selection onto the row it had
/// just found therefore read that answer, dropped the row out of the filter,
/// and left `d` reporting that nothing matches a row still drawn with `✓`. A
/// filter is a view: it hides rows without choosing a conversation.
#[test]
fn the_done_filter_keeps_the_row_it_found_and_leaves_the_open_conversation_alone() {
    let mut dashboard = dashboard_with_an_unread_reply_off_screen();
    let lines = drawn(&mut dashboard, 120, 40);
    let row = lines
        .iter()
        .find(|line| line.contains("beta"))
        .expect("beta has a row");
    assert!(
        row.contains(mj_chat::theme::glyphs().unread),
        "the row says the answer is unread: {lines:#?}"
    );
    dashboard.focus_sessions();

    dashboard.handle_key(key(KeyCode::Char('d')));

    assert_eq!(
        dashboard
            .ordered_sessions()
            .into_iter()
            .map(|session| session.id.clone())
            .collect::<Vec<_>>(),
        ["beta"]
    );
    assert_eq!(
        dashboard.selected_session_id(),
        Some("alpha"),
        "the filter must not hand the host a different conversation to open"
    );
}

/// A letter that hides every row leaves the pane with no row to select, so the
/// frame moves the focus to the Create action. The filter must keep answering
/// from there: the empty pane's own line promises that Esc clears the filter,
/// and without the letters there is no way back to the sessions at all.
#[test]
fn a_state_letter_that_hides_every_row_keeps_answering_the_letters_and_esc() {
    let mut dashboard = dashboard_with_attention_mix();
    dashboard.set_active_workspace(Some("default".into()));
    dashboard.focus_sessions();
    let ids = |dashboard: &DashboardState| {
        dashboard
            .ordered_sessions()
            .into_iter()
            .map(|session| session.id.clone())
            .collect::<Vec<_>>()
    };

    // Nothing is working, so `w` empties the list.
    dashboard.handle_key(key(KeyCode::Char('w')));
    let lines = drawn(&mut dashboard, 120, 40);
    assert!(ids(&dashboard).is_empty());
    assert!(
        lines.iter().any(|line| line.contains("No sessions match")),
        "{lines:#?}"
    );
    assert_eq!(
        dashboard.session_action_focus,
        Some(CommandId::NewSessionWizard),
        "an empty list leaves the focus on the action row"
    );

    // `a` widens the filter to everything again, and the rows take the focus
    // back from the action row.
    dashboard.handle_key(key(KeyCode::Char('a')));
    assert!(dashboard.sessions_filter.is_none());
    assert_eq!(ids(&dashboard), ["asks", "done", "quiet"]);
    assert_eq!(dashboard.session_action_focus, None);

    // Another letter still narrows from the empty pane, rather than leaving
    // the person with one filter and no way to change it.
    dashboard.handle_key(key(KeyCode::Char('w')));
    let _ = drawn(&mut dashboard, 120, 40);
    dashboard.handle_key(key(KeyCode::Char('b')));
    assert_eq!(ids(&dashboard), ["asks"]);

    // And Esc drops the filter the pane says it drops.
    dashboard.handle_key(key(KeyCode::Char('w')));
    let _ = drawn(&mut dashboard, 120, 40);
    dashboard.handle_key(key(KeyCode::Esc));
    assert!(dashboard.sessions_filter.is_none());
    assert_eq!(ids(&dashboard), ["asks", "done", "quiet"]);
}

#[test]
fn the_palette_finds_create_session_from_cre_and_lists_recent_commands_first() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.focus_sessions();
    open_palette(&mut dashboard);
    for character in "cre".chars() {
        dashboard.handle_key(key(KeyCode::Char(character)));
    }
    let Mode::Palette(palette) = &dashboard.mode else {
        panic!("palette open");
    };
    assert_eq!(palette.entries[0].id, CommandId::NewSessionWizard);
    dashboard.handle_key(key(KeyCode::Esc));

    // A subsequence query ranks a word-start match above a description hit.
    open_palette(&mut dashboard);
    for character in "mvs".chars() {
        dashboard.handle_key(key(KeyCode::Char(character)));
    }
    let Mode::Palette(palette) = &dashboard.mode else {
        panic!("palette open");
    };
    assert_eq!(
        palette.entries[0].id,
        CommandId::MoveSession,
        "{:?}",
        palette.entries
    );
    dashboard.handle_key(key(KeyCode::Esc));

    // Running a command puts it under Recent the next time the palette opens
    // with an empty query.
    chord(&mut dashboard, CommandId::MarkAllRead);
    open_palette(&mut dashboard);
    let lines = drawn(&mut dashboard, 120, 40);
    let recent = lines
        .iter()
        .position(|line| line.contains("Recent"))
        .expect("Recent heading");
    assert!(lines[recent + 1].contains("Mark all read"), "{lines:#?}");
}

#[test]
fn idle_suspension_runs_immediately_and_working_suspension_warns() {
    let mut session = running_session();
    session.project_directory = Some("/srv/project".into());
    let mut dashboard = dashboard_with_session(session);
    dashboard.focus_sessions();
    assert_eq!(
        dashboard.dispatch_command(CommandId::SuspendSession),
        DashboardAction::Suspend {
            session_id: "session-1".into(),
            acknowledge_unpublished_work: false,
        }
    );
    assert!(matches!(dashboard.mode, Mode::Dashboard));
    // Working suspension also explains that it interrupts the turn.
    dashboard
        .session_details
        .get_mut("session-1")
        .unwrap()
        .current_turn_started_at = Some(1);
    assert_eq!(
        dashboard.attention_level("session-1"),
        AttentionLevel::Working
    );
    assert_eq!(
        dashboard.dispatch_command(CommandId::SuspendSession),
        DashboardAction::None
    );
    assert!(matches!(dashboard.mode, Mode::Confirm(_)));
    let lines = drawn(&mut dashboard, 120, 40);
    assert!(
        lines
            .iter()
            .any(|line| line.contains("The current turn will be interrupted.")),
        "{lines:#?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("c Cancel") && line.contains("s Suspend session")),
        "{lines:#?}"
    );
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Char('s'))),
        DashboardAction::Suspend {
            session_id: "session-1".into(),
            acknowledge_unpublished_work: true,
        }
    );
    assert!(matches!(dashboard.mode, Mode::Dashboard));

    assert_eq!(
        dashboard.dispatch_command(CommandId::RestartSession),
        DashboardAction::None
    );
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Char('c'))),
        DashboardAction::None
    );
    assert!(matches!(dashboard.mode, Mode::Dashboard));
}

#[test]
fn idle_managed_clone_suspension_requires_publication_confirmation() {
    let mut session = running_session();
    let root = std::path::PathBuf::from("/srv/project/.mj/clones/session-1");
    session.project_directory = Some(root.clone());
    session.managed_worktree = Some(mj_core::state::ManagedWorktree {
        kind: mj_core::state::ManagedCheckoutKind::Clone,
        source_project_directory: "/srv/project".into(),
        source_repository: "/srv/project".into(),
        worktree_root: root,
        branch: "master".into(),
        target: mj_core::state::ManagedWorktreeTarget::Local,
        base_commit: Some("1".repeat(40)),
    });
    let mut dashboard = dashboard_with_session(session);
    dashboard.focus_sessions();
    assert_eq!(
        dashboard.dispatch_command(CommandId::SuspendSession),
        DashboardAction::None
    );
    let lines = drawn(&mut dashboard, 120, 40);
    assert!(
        lines
            .iter()
            .any(|line| line.contains("Publication status is unverified"))
    );
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Char('s'))),
        DashboardAction::Suspend {
            session_id: "session-1".into(),
            acknowledge_unpublished_work: true,
        }
    );
}

#[test]
fn every_confirmation_button_answers_a_unique_letter() {
    use crate::dialogs::render::confirmation_accelerators;
    assert_eq!(
        confirmation_accelerators(&["No", "Yes", "Yes, delete branch"]),
        ['n', 'y', 'd']
    );
    assert_eq!(
        confirmation_accelerators(&["Cancel", "Confirm"]),
        ['c', 'o']
    );
    assert_eq!(
        confirmation_accelerators(&["Dismiss", "Open transcript", "Open settings"]),
        ['d', 'o', 's']
    );
    assert_eq!(
        confirmation_accelerators(&["Cancel", "Force stop", "Retry suspension"]),
        ['c', 'f', 'r']
    );

    // The delete dialog's third button is reachable by its letter.
    let mut dashboard = dashboard_with_session(legacy_managed_session(running_session()));
    dashboard.focus_sessions();
    dashboard.dispatch_command(CommandId::DestroySession);
    let lines = drawn(&mut dashboard, 120, 40);
    assert!(
        lines
            .iter()
            .any(|line| line.contains("a Destroy and delete branch")),
        "{lines:#?}"
    );
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Char('a'))),
        DashboardAction::ForceDestroy {
            session_id: "session-1".into(),
            delete_branch: true
        }
    );
}

#[test]
fn esc_clears_a_help_filter_then_closes_help_and_clears_a_pane_notice() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.focus_sessions();
    chord(&mut dashboard, CommandId::Help);
    dashboard.handle_key(key(KeyCode::Char('x')));
    dashboard.handle_key(key(KeyCode::Esc));
    assert!(
        matches!(&dashboard.mode, Mode::Help(overlay) if overlay.query.is_empty()),
        "{:?}",
        dashboard.mode
    );
    dashboard.handle_key(key(KeyCode::Esc));
    assert_eq!(dashboard.mode, Mode::Dashboard);

    dashboard.set_notice("Something happened.");
    assert!(dashboard.notices.current().is_some());
    dashboard.handle_key(key(KeyCode::Esc));
    assert_eq!(dashboard.notices.current(), None);
}

fn git_status_fixture() -> mj_core::local_git::SessionGitStatus {
    mj_core::local_git::parse_git_status(
        std::path::PathBuf::from("/work"),
        "feature/x",
        Some("2\t1"),
        "12\t3\tsrc/main.rs\n",
        " M src/main.rs\n?? notes.md\n",
    )
}

#[test]
fn session_rows_carry_the_branch_once_the_checkout_was_read() {
    let mut session = running_session();
    session.target = Some(mj_core::state::TargetLocator::LocalBare {
        worker_root: "/work".into(),
    });
    let mut dashboard = dashboard_with_session(session);
    dashboard.focus_sessions();
    let lines = drawn(&mut dashboard, 120, 40);
    assert!(!lines.iter().any(|line| line.contains("⎇")), "{lines:#?}");

    dashboard.set_git_status("session-1".into(), Ok(git_status_fixture()));
    let lines = drawn(&mut dashboard, 160, 40);
    assert!(
        lines
            .iter()
            .any(|line| line.contains("ACP pretty name  ⎇ feature/x ↑1 ↓2 ±2")),
        "{lines:#?}"
    );
    // A narrow sidebar keeps the branch and drops the counts.
    let lines = drawn(&mut dashboard, 100, 40);
    assert!(
        lines
            .iter()
            .any(|line| line.contains("ACP pretty name  ⎇ feature/x") && !line.contains("±2")),
        "{lines:#?}"
    );
    // A checkout that is not a repository adds nothing to the row.
    dashboard.set_git_status(
        "session-1".into(),
        Ok(mj_core::local_git::parse_git_status(
            "/work".into(),
            "not a git checkout",
            None,
            "",
            "",
        )),
    );
    let lines = drawn(&mut dashboard, 120, 40);
    assert!(!lines.iter().any(|line| line.contains("⎇")), "{lines:#?}");
}

/// Every session created with a managed worktree gets a `mj/<32 hex>` branch,
/// which is wider than the sidebar, so dropping the whole marker hid the
/// feature at the widths people use. The middle of the name is what nobody
/// reads: elide it and the marker keeps its ahead, behind, and changed counts.
#[test]
fn a_long_branch_name_is_elided_in_the_middle_so_the_marker_keeps_its_counts() {
    let mut session = running_session();
    session.session_title_override = Some("alpha".into());
    session.target = Some(mj_core::state::TargetLocator::LocalBare {
        worker_root: "/work".into(),
    });
    let mut dashboard = dashboard_with_session(session);
    dashboard.focus_sessions();
    // The 35 characters a managed worktree's own branch name costs.
    let branch = "mj/d45dc90af02510176be667cf8b330580";
    assert_eq!(branch.chars().count(), 35);
    dashboard.set_git_status(
        "session-1".into(),
        Ok(mj_core::local_git::parse_git_status(
            "/work".into(),
            branch,
            Some("2\t1"),
            "12\t3\tsrc/main.rs\n",
            " M src/main.rs\n?? notes.md\n",
        )),
    );
    // A third of a 100-column terminal, floored at 40, is a 40-column pane.
    let lines = drawn(&mut dashboard, 100, 40);
    let row = lines
        .iter()
        .find(|line| line.contains("alpha"))
        .unwrap_or_else(|| panic!("the session row: {lines:#?}"));
    // The prefix and the last characters of the name survive; the middle does
    // not, which is what makes room for the counts.
    assert!(row.contains("⎇ mj/d45"), "{row:?}");
    assert!(row.contains("…"), "{row:?}");
    assert!(row.contains("80 ↑1 ↓2 ±2"), "{row:?}");
}

#[test]
fn git_probes_cover_visible_live_sessions_about_once_a_minute() {
    let mut session = running_session();
    session.target = Some(mj_core::state::TargetLocator::LocalBare {
        worker_root: "/work".into(),
    });
    let mut dashboard = dashboard_with_session(session);
    let start = std::time::Instant::now();
    assert_eq!(dashboard.git_probe_candidates(start, 2), ["session-1"]);
    assert!(dashboard.git_probe_candidates(start, 2).is_empty());
    assert!(
        dashboard
            .git_probe_candidates(start + std::time::Duration::from_secs(30), 2)
            .is_empty()
    );
    assert_eq!(
        dashboard.git_probe_candidates(start + std::time::Duration::from_secs(61), 2),
        ["session-1"]
    );
    // A stopped session's target is gone, so there is nothing to read.
    dashboard.state.sessions.get_mut("session-1").unwrap().state = SessionState::Stopped;
    assert!(
        dashboard
            .git_probe_candidates(start + std::time::Duration::from_secs(200), 2)
            .is_empty()
    );
}

/// Launch finding D-3: after a restart every session is due at once. The
/// host reads only a few per pass, so a session it did not read must stay
/// due; marking all of them as read left all but the first few without a
/// branch marker for good.
#[test]
fn git_probes_reach_every_session_when_more_are_due_than_one_pass_reads() {
    let target = mj_core::state::TargetLocator::LocalBare {
        worker_root: "/work".into(),
    };
    let mut first = running_session();
    first.target = Some(target.clone());
    let mut dashboard = dashboard_with_session(first);
    for id in ["session-2", "session-3", "session-4"] {
        let mut session = running_session();
        session.id = id.into();
        session.target = Some(target.clone());
        dashboard.state.sessions.insert(id.into(), session);
    }
    let start = std::time::Instant::now();

    let mut probed = dashboard.git_probe_candidates(start, 2);
    assert_eq!(probed.len(), 2, "one pass reads at most the limit");
    probed.extend(dashboard.git_probe_candidates(start + std::time::Duration::from_secs(1), 2));
    probed.sort();

    assert_eq!(probed, ["session-1", "session-2", "session-3", "session-4"]);
}

#[test]
fn the_changed_files_overlay_lists_files_and_refreshes_on_r() {
    let mut session = running_session();
    session.target = Some(mj_core::state::TargetLocator::LocalBare {
        worker_root: "/work".into(),
    });
    let mut dashboard = dashboard_with_session(session);
    dashboard.focus_sessions();
    assert_eq!(
        chord(&mut dashboard, CommandId::ChangedFiles),
        DashboardAction::ProbeGitStatus {
            session_id: "session-1".into()
        }
    );
    assert!(matches!(dashboard.mode, Mode::ChangedFiles(_)));
    let lines = drawn(&mut dashboard, 120, 40);
    assert!(
        lines
            .iter()
            .any(|line| line.contains("Reading the checkout")),
        "{lines:#?}"
    );

    dashboard.set_git_status("session-1".into(), Ok(git_status_fixture()));
    let lines = drawn(&mut dashboard, 120, 40);
    // The longest status word fills its column, so the column has to carry the
    // separator: `modifiedsrc/main.rs` is not a line anyone can read.
    assert!(
        lines
            .iter()
            .any(|line| line.contains("modified src/main.rs") && line.contains("+12 −3")),
        "{lines:#?}"
    );
    assert!(
        lines.iter().any(|line| line.contains("new      notes.md")),
        "{lines:#?}"
    );
    assert!(
        lines.iter().any(|line| line.contains("2 files · +12 −3")),
        "{lines:#?}"
    );

    assert_eq!(
        dashboard.handle_key(key(KeyCode::Char('r'))),
        DashboardAction::ProbeGitStatus {
            session_id: "session-1".into()
        }
    );
    dashboard.handle_key(key(KeyCode::Esc));
    assert_eq!(dashboard.mode, Mode::Dashboard);

    // Without a running target the command explains itself instead of
    // opening an overlay that can never fill.
    dashboard
        .state
        .sessions
        .get_mut("session-1")
        .unwrap()
        .target = None;
    let lines = drawn(&mut dashboard, 120, 40);
    assert!(!lines.iter().any(|line| line.contains("Changed files ·")));
    assert!(matches!(
        (crate::actions::spec(CommandId::ChangedFiles).available)(&dashboard),
        crate::actions::Availability::Blocked(_)
    ));
}

#[test]
fn the_ascii_symbol_set_draws_the_dashboard_without_non_ascii_glyphs() {
    let mut dashboard = dashboard_with_attention_mix();
    dashboard.set_active_workspace(Some("default".into()));
    dashboard.set_git_status("asks".into(), Ok(git_status_fixture()));
    let mut config = dashboard.config.clone();
    config.advanced.symbols = Some(mj_core::config::SymbolSet::Ascii);
    dashboard.set_config(config);
    let lines = drawn(&mut dashboard, 120, 40);
    let offenders = lines
        .iter()
        .flat_map(|line| line.chars())
        .filter(|character| !character.is_ascii())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(
        offenders.is_empty(),
        "non-ASCII glyphs drawn: {offenders:?}\n{lines:#?}"
    );
    assert!(
        lines.iter().any(|line| line.contains("+---")),
        "ASCII borders: {lines:#?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("ACP pretty name  br feature/x")),
        "{lines:#?}"
    );
    let wide = drawn(&mut dashboard, 160, 40);
    assert!(
        wide.iter()
            .any(|line| line.contains("br feature/x +1 -2 ~2")),
        "{wide:#?}"
    );
    // The chord hints are joined by the ASCII separator.
    assert!(
        wide.last().unwrap().contains("c create - g sessions"),
        "ASCII footer separators: {}",
        wide.last().unwrap()
    );

    // The default set is unchanged.
    let mut config = dashboard.config.clone();
    config.advanced.symbols = Some(mj_core::config::SymbolSet::Unicode);
    dashboard.set_config(config);
    let lines = drawn(&mut dashboard, 120, 40);
    assert!(lines.iter().any(|line| line.contains("╭")), "{lines:#?}");
}

#[test]
fn the_monochrome_theme_draws_every_surface_without_colors() {
    let mut dashboard = dashboard_with_attention_mix();
    dashboard.set_active_workspace(Some("default".into()));
    let mut config = dashboard.config.clone();
    config.theme = mj_core::config::UiTheme::Mono;
    dashboard.set_config(config);
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let colored = buffer
        .content()
        .iter()
        .filter(|cell| {
            cell.fg != ratatui::style::Color::Reset || cell.bg != ratatui::style::Color::Reset
        })
        .count();
    assert_eq!(colored, 0, "monochrome must paint no colors");
    // The selected row still stands out, by reverse video.
    let lines = buffer_lines(buffer);
    let (column, row) = point(&lines, "› ");
    assert!(
        buffer[(column, row)]
            .modifier
            .contains(ratatui::style::Modifier::REVERSED),
        "selection is reverse video in mono"
    );
    open_palette(&mut dashboard);
    drawn(&mut dashboard, 120, 40);
    dashboard.handle_key(key(KeyCode::Esc));
    chord(&mut dashboard, CommandId::Help);
    drawn(&mut dashboard, 120, 40);
}

#[test]
fn the_notice_log_lists_notices_newest_first_and_stacked_failures() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.focus_sessions();
    dashboard.set_notice("Profile quotas refreshed.");
    dashboard.set_failure_notice("Resume failed: archive missing");
    dashboard.set_failure_notice("Move failed: target unreachable");
    let lines = drawn(&mut dashboard, 120, 40);
    assert!(
        lines
            .last()
            .unwrap()
            .contains("2 failures · latest: Move failed: target unreachable"),
        "{}",
        lines.last().unwrap()
    );
    dashboard.dispatch_command(CommandId::NoticeLog);
    assert!(matches!(dashboard.mode, Mode::NoticeLog(_)));
    let lines = drawn(&mut dashboard, 120, 40);
    let newest = lines
        .iter()
        .position(|line| line.contains("Move failed: target unreachable"))
        .expect("newest failure");
    let older = lines
        .iter()
        .position(|line| line.contains("Resume failed: archive missing"))
        .expect("older failure");
    let oldest = lines
        .iter()
        .position(|line| line.contains("Profile quotas refreshed."))
        .expect("plain notice");
    assert!(newest < older && older < oldest, "{lines:#?}");
    assert!(lines[newest].contains("ago"), "{lines:#?}");
    dashboard.handle_key(key(KeyCode::Esc));
    assert_eq!(dashboard.mode, Mode::Dashboard);
}

#[test]
fn a_failed_session_shows_its_error_instead_of_an_attach_that_cannot_finish() {
    let mut session = running_session();
    session.state = SessionState::Error;
    session.last_error = Some("connect worker at /var/lib/hel/workers/abc: no such file".into());
    let mut dashboard = dashboard_with_session(session);
    dashboard.focus_sessions();
    assert!(dashboard.session_failed("session-1"));
    let lines = drawn(&mut dashboard, 120, 40);
    let text = lines.join("\n");
    assert!(text.contains("Session failed"), "{lines:#?}");
    assert!(text.contains("connect worker at"), "{lines:#?}");
    assert!(
        text.contains("Enter in Sessions opens its transcript or recovers it"),
        "{lines:#?}"
    );
    assert!(!text.contains("Bringing your conversation"), "{lines:#?}");
    // Enter still asks what to do rather than attaching blindly.
    dashboard.handle_key(key(KeyCode::Enter));
    assert!(
        matches!(dashboard.mode, Mode::Confirm(_)),
        "{:?}",
        dashboard.mode
    );
}

#[test]
fn a_daemon_notice_about_another_workspace_names_that_workspace_first() {
    let mut dashboard = dashboard_with_attention_mix();
    dashboard.set_active_workspace(Some("default".into()));
    assert_eq!(
        dashboard.notice_naming_workspace("remote", "Credential sync failed".into()),
        "In workspace Other: Credential sync failed"
    );
    assert_eq!(
        dashboard.notice_naming_workspace("asks", "Credential sync failed".into()),
        "Credential sync failed"
    );
    assert_eq!(
        dashboard.notice_naming_workspace("missing", "Credential sync failed".into()),
        "Credential sync failed"
    );
}

#[test]
fn the_notice_log_wraps_a_long_failure_instead_of_cutting_its_tail() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.focus_sessions();
    let long = format!(
        "Credential sync for profile codex (session 2ac006c1) failed: relay proxy stderr (last 4 lines): {} the-very-last-word",
        "x".repeat(150)
    );
    dashboard.set_failure_notice(long);
    dashboard.dispatch_command(CommandId::NoticeLog);
    let lines = drawn(&mut dashboard, 120, 40);
    let text = lines.join("\n");
    assert!(text.contains("the-very-last-word"), "{lines:#?}");
    let first = lines
        .iter()
        .position(|line| line.contains("Credential sync for profile"))
        .expect("the message starts");
    assert!(lines[first].contains("ago"), "{lines:#?}");
    assert!(
        lines[first + 1].contains("xxxx") || lines[first + 2].contains("xxxx"),
        "continuation lines under the age column: {lines:#?}"
    );
}

/// R10-2: a native Codex child that had finished while its parent was still
/// running read "No messages yet" in the Sub-agents view, its conversation
/// never appeared, and Enter did nothing, although the store held its
/// transcript. The host empties any pane whose session
/// `pane_session_is_suspended` calls suspended (R4-11), and a finished native
/// child's presentation row is `Stopped`, so Browse let it go as soon as the
/// selection put it there.
#[test]
fn a_finished_native_child_shows_its_stored_transcript_and_opens_on_enter() {
    let (mut dashboard, parent_id, id) = dashboard_with_finished_native_child();
    dashboard.open_subagent_workspace(parent_id);
    assert_eq!(dashboard.selected_session_id(), Some(id.as_str()));
    assert!(
        !dashboard.pane_session_is_suspended(&id),
        "a finished native child is read from the store; no pane lets it go"
    );

    // Browse follows the selection onto the child.
    let browse = dashboard.browse_pane();
    dashboard.set_pane_session(browse, Some(&id));
    let lines = drawn(&mut dashboard, 140, 40);
    let screen = lines.join("\n");
    assert!(
        screen.contains("Reading calc.py") && screen.contains("Native agent"),
        "the child's stored transcript is drawn in its pane: {screen}"
    );
    let row = lines
        .iter()
        .position(|line| line.contains("Review calc · completed"))
        .expect("the child's row");
    let row_text = lines[row..row + 4].join("\n");
    assert!(
        !row_text.contains("No messages yet"),
        "the row shows the child's last message: {row_text}"
    );
    assert!(
        row_text.contains("calc.py adds and subtracts"),
        "{row_text}"
    );

    // Enter on the row opens the child's conversation.
    dashboard.focus_sessions();
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::Open {
            session_id: id.clone()
        }
    );
}

#[test]
fn native_agent_pane_survives_refresh_and_blocks_managed_session_actions() {
    use mj_core::native_agent::*;
    let parent = running_session();
    let mut dashboard = dashboard_with_session(parent.clone());
    let agent = NativeAgent {
        availability: Default::default(),
        availability_reason: None,
        stable_id: None,
        owner_session_id: parent.id.clone(),
        session_id: "native-child".into(),
        parent_session_id: None,
        name: "Inspect parser".into(),
        task: "Find parser errors".into(),
        capabilities: NativeAgentCapabilities {
            cancel: true,
            close: false,
        },
        state: NativeAgentState::Running,
    };
    let id = agent.view_id();
    let projection = mj_core::state::MaterializedSession::empty(&id);
    dashboard.set_native_agents(vec![NativeAgentView {
        generation_ordinal: 1,
        agent,
        projection,
    }]);
    assert_eq!(dashboard.subagent_count_for(&parent.id), 1);
    assert!(!dashboard.ordered_sessions().iter().any(|row| row.id == id));
    dashboard.open_subagent_workspace(parent.id.clone());
    assert_eq!(dashboard.selected_session_id(), Some(id.as_str()));
    assert_eq!(dashboard.ordered_sessions().len(), 1);
    let mut state = State::default();
    state.sessions.insert(parent.id.clone(), parent.clone());
    dashboard.set_state(state);
    assert_eq!(dashboard.selected_session_id(), Some(id.as_str()));
    assert!(matches!(
        dashboard.dispatch_command(crate::actions::CommandId::SuspendSession),
        DashboardAction::None
    ));
    assert_eq!(
        chord(&mut dashboard, CommandId::SuspendSession),
        DashboardAction::None
    );
    assert!(
        matches!(dashboard.mode, Mode::Dashboard),
        "native agents close through their parent"
    );
    assert!(
        matches!(dashboard.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)), DashboardAction::StopNativeAgent { owner, child } if owner == parent.id && child == "native-child")
    );
    assert!(matches!(
        dashboard.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)),
        DashboardAction::None
    ));
    dashboard.native_agent_stop_finished(&parent.id, "native-child", Err("offline".into()));
    assert!(matches!(
        dashboard.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)),
        DashboardAction::StopNativeAgent { .. }
    ));
    // Returning to a managed owner retains that owner's own parent workspace.
    let mut ancestor = parent.clone();
    ancestor.id = "ancestor".into();
    dashboard
        .state
        .sessions
        .insert(ancestor.id.clone(), ancestor.clone());
    dashboard.state.subagents.insert(
        parent.id.clone(),
        mj_core::subagent::SubagentRecord {
            child_session_id: parent.id.clone(),
            parent_session_id: ancestor.id.clone(),
            task_name: "managed owner".into(),
            profile_id: parent.last_profile.clone(),
            model: None,
            effort: None,
            working_directory: Default::default(),
            initial_prompt: "inspect".into(),
            request_key: "request".into(),
            created_at: parent.created_at.clone(),
            noticed_turn: None,
            handback_tool: false,
        },
    );
    assert!(
        matches!(dashboard.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE)), DashboardAction::Open { session_id } if session_id == parent.id)
    );
    assert_eq!(dashboard.subagent_parent_id(), Some("ancestor"));
    assert_eq!(dashboard.selected_session_id(), Some(parent.id.as_str()));
}

#[test]
fn resize_mode_routes_repeated_motions_consumes_text_and_exits() {
    let mut dashboard = dashboard_with_two_sessions();
    dashboard
        .split_focused_pane(ratatui::layout::Direction::Horizontal, Some("session-2"))
        .unwrap();
    dashboard.focus = Focus::Prompt;
    chord(&mut dashboard, CommandId::ResizeMode);
    assert!(dashboard.resize_mode_active());
    let before = dashboard.conversation_layout_for("default");
    let repeat = KeyEvent {
        kind: crossterm::event::KeyEventKind::Repeat,
        ..key(KeyCode::Char('h'))
    };
    route(&mut dashboard, &[repeat, repeat]);
    assert!(dashboard.resize_mode_active());
    assert_ne!(dashboard.conversation_layout_for("default"), before);
    assert_eq!(
        dashboard.route_bound_key(&key(KeyCode::Char('q'))),
        KeyRoute::Consumed
    );
    assert_eq!(
        dashboard.route_bound_key_event(&Event::Paste("do not submit".into())),
        KeyRoute::Consumed
    );
    let lines = drawn(&mut dashboard, 120, 40);
    assert!(lines.last().unwrap().contains("Resize panes:"));
    route(&mut dashboard, &[key(KeyCode::Esc)]);
    assert!(!dashboard.resize_mode_active());
    assert_eq!(
        dashboard.route_bound_key(&key(KeyCode::Char('h'))),
        KeyRoute::Forward
    );
    chord(&mut dashboard, CommandId::ResizeMode);
    chord(&mut dashboard, CommandId::Help);
    assert!(!dashboard.resize_mode_active());
    dashboard.cancel_modal();
    assert!(!dashboard.resize_mode_active());
}

#[test]
fn resize_mode_respects_custom_commands_and_workspace_changes() {
    let mut dashboard = dashboard_with_two_sessions();
    let mut configured = config();
    configured.keys.resize_mode = "prefix+e".into();
    configured.keys.refresh = "f5".into();
    dashboard.set_config(configured);
    assert_eq!(
        chord(&mut dashboard, CommandId::ResizeMode),
        DashboardAction::None
    );
    assert!(
        !dashboard.resize_mode_active(),
        "a single pane cannot resize"
    );
    dashboard
        .split_focused_pane(ratatui::layout::Direction::Horizontal, Some("session-2"))
        .unwrap();
    chord(&mut dashboard, CommandId::ZoomPane);
    assert!(dashboard.conversation_zoomed());
    chord(&mut dashboard, CommandId::ResizeMode);
    assert!(!dashboard.conversation_zoomed());
    assert_eq!(
        route(&mut dashboard, &[key(KeyCode::F(5))]),
        DashboardAction::RefreshAll
    );
    assert!(!dashboard.resize_mode_active());
    chord(&mut dashboard, CommandId::ResizeMode);
    dashboard.set_active_workspace(None);
    assert!(!dashboard.resize_mode_active());
}

#[test]
fn swapping_nested_panes_moves_focus_and_sessions_without_changing_ratios() {
    let mut dashboard = dashboard_with_two_sessions();
    dashboard.set_current_session(Some("session-1"));
    let first = dashboard.focused_pane();
    let second = dashboard
        .split_focused_pane(ratatui::layout::Direction::Horizontal, Some("session-2"))
        .unwrap();
    let third = dashboard
        .split_focused_pane(ratatui::layout::Direction::Vertical, None)
        .unwrap();
    dashboard.focus_pane(first);
    let before = dashboard.conversation_panes(dashboard.conversation_area());
    chord(&mut dashboard, CommandId::SwapPaneRight);
    assert_eq!(dashboard.focused_pane(), first);
    assert_eq!(dashboard.pane_session(first), Some("session-1"));
    assert_eq!(dashboard.pane_session(second), Some("session-2"));
    assert_eq!(dashboard.pane_session(third), None);
    let after = dashboard.conversation_panes(dashboard.conversation_area());
    assert_eq!(
        before.iter().map(|p| p.rect).collect::<Vec<_>>(),
        after.iter().map(|p| p.rect).collect::<Vec<_>>()
    );
    assert_ne!(
        before.iter().find(|p| p.id == first).unwrap().rect,
        after.iter().find(|p| p.id == first).unwrap().rect
    );
    let saved = dashboard.conversation_layout_for("default");
    let mut restored = dashboard_with_two_sessions();
    restored.cache_workspace_layout("default", saved.clone());
    assert_eq!(restored.conversation_layout_for("default"), saved);
    assert_eq!(restored.focused_pane(), first);
    assert_eq!(restored.pane_session(first), Some("session-1"));
    chord(&mut dashboard, CommandId::SwapPaneRight);
    assert_eq!(
        dashboard.conversation_layout_for("default"),
        saved,
        "no neighbor is a no-op"
    );
}

/// Launch finding R11-1: a Claude sub-agent sat on a permission question in
/// the Sub-agents view while its parent's row read only "Working" with no
/// attention mark, so nobody knew to look. A child's question marks its parent
/// the way the parent's own question would: the row's symbol, the attention
/// queue and a notification, and the row says whose question it is.
#[test]
fn a_subagent_question_marks_its_parent_for_attention() {
    let (mut dashboard, parent) = dashboard_with_one_subagent();
    dashboard
        .state
        .sessions
        .get_mut("child-session")
        .unwrap()
        .acp_session_title = Some("Answer project codename".into());
    set_working(&mut dashboard, &parent);
    set_working(&mut dashboard, "child-session");
    dashboard.set_current_session(None);
    assert_eq!(dashboard.attention_level(&parent), AttentionLevel::Working);
    assert!(dashboard.notification_events(0).is_empty());

    dashboard
        .session_details
        .get_mut("child-session")
        .unwrap()
        .pending_elicitations = vec![question("child-session")];

    assert_eq!(dashboard.attention_level(&parent), AttentionLevel::Waiting);
    assert_eq!(
        dashboard
            .attention_queue()
            .iter()
            .map(|entry| entry.session_id.as_str())
            .collect::<Vec<_>>(),
        [parent.as_str()],
        "the queue lists top-level rows, so it leads to the parent"
    );
    let lines = drawn(&mut dashboard, 120, 40);
    let row = lines
        .iter()
        .position(|line| line.contains("ACP pretty name"))
        .expect("the parent's row is drawn");
    assert!(
        lines[row].contains(&format!(
            "{} ACP pretty name",
            mj_chat::theme::glyphs().waiting
        )),
        "{}",
        lines[row]
    );
    assert!(
        lines[row + 1].contains("Sub-agent question"),
        "{}",
        lines[row + 1]
    );
    assert!(dashboard.notification_events(0).is_empty());
    let due = dashboard.notification_events(2_000);
    assert_eq!(due.len(), 1, "{due:?}");
    assert_eq!(due[0].session_id, parent);
    assert_eq!(due[0].level, AttentionLevel::Waiting);
    assert_eq!(
        due[0].body,
        "Sub-agent \"Answer project codename\": Choose a path"
    );

    // Answering the child's question clears the parent's mark.
    dashboard
        .session_details
        .get_mut("child-session")
        .unwrap()
        .pending_elicitations
        .clear();
    assert_eq!(dashboard.attention_level(&parent), AttentionLevel::Working);
}

#[test]
fn idle_parent_suspension_confirms_when_a_subagent_is_active() {
    let mut dashboard = dashboard_with_session(running_session());
    let mut child = running_session();
    child.id = "child".into();
    dashboard.state.subagents.insert(
        child.id.clone(),
        mj_core::subagent::SubagentRecord {
            child_session_id: child.id.clone(),
            parent_session_id: "session-1".into(),
            task_name: "child task".into(),
            profile_id: child.last_profile.clone(),
            model: None,
            effort: None,
            working_directory: Default::default(),
            initial_prompt: "inspect".into(),
            request_key: "request".into(),
            created_at: child.created_at.clone(),
            noticed_turn: None,
            handback_tool: false,
        },
    );
    dashboard.state.sessions.insert(child.id.clone(), child);
    assert_eq!(dashboard.attention_level("session-1"), AttentionLevel::Idle);
    assert_eq!(
        chord(&mut dashboard, CommandId::SuspendSession),
        DashboardAction::None
    );
    assert!(matches!(
        &dashboard.mode,
        Mode::Confirm(dialog) if matches!(
            dialog.confirmation,
            crate::dialogs::Confirmation::SuspendSession {
                active_children: 1,
                interrupting: false,
                ..
            }
        )
    ));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Esc)),
        DashboardAction::None
    );
    assert!(matches!(dashboard.mode, Mode::Dashboard));
}

/// Launch finding B-3: the row menu is titled with the session's name, so
/// the Suspend confirmation and the Rename dialog must name it the same way
/// instead of by its id.
#[test]
fn suspend_confirmation_and_rename_dialog_name_the_session_by_its_title() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard
        .session_details
        .get_mut("session-1")
        .unwrap()
        .current_turn_started_at = Some(1);
    chord(&mut dashboard, CommandId::SuspendSession);
    let suspend = drawn(&mut dashboard, 120, 40).join("\n");
    assert!(suspend.contains("Session: ACP pretty name"), "{suspend}");
    assert!(!suspend.contains("Session: session-1"), "{suspend}");
    dashboard.handle_key(key(KeyCode::Esc));

    chord(&mut dashboard, CommandId::RenameSession);
    let rename = drawn(&mut dashboard, 120, 40).join("\n");
    assert!(rename.contains("Session: ACP pretty name"), "{rename}");
    assert!(!rename.contains("Session: session-1"), "{rename}");
}

/// R4-12: a session nobody has named yet keeps the title it was created with
/// ("project via claude"), and the session list shows that title (R2-8). The
/// palette heading, the Suspend confirmation and the Rename dialog named it
/// by its id instead.
#[test]
fn dialogs_name_an_unnamed_session_by_the_title_it_was_created_with() {
    let mut session = running_session();
    session.acp_session_title = None;
    session.title = "project via claude".into();
    let mut dashboard = dashboard_with_session(session);
    dashboard
        .session_details
        .get_mut("session-1")
        .unwrap()
        .current_turn_started_at = Some(1);

    dashboard.begin_session_palette();
    let palette = drawn(&mut dashboard, 120, 40).join("\n");
    assert!(palette.contains("project via claude"), "{palette}");
    dashboard.handle_key(key(KeyCode::Esc));

    chord(&mut dashboard, CommandId::SuspendSession);
    let suspend = drawn(&mut dashboard, 120, 40).join("\n");
    assert!(suspend.contains("Session: project via claude"), "{suspend}");
    dashboard.handle_key(key(KeyCode::Esc));

    chord(&mut dashboard, CommandId::RenameSession);
    let rename = drawn(&mut dashboard, 120, 40).join("\n");
    assert!(rename.contains("Session: project via claude"), "{rename}");
    assert!(!rename.contains("Session: session-1"), "{rename}");
}

/// A session with no name at all falls back to its id, the only name it has.
#[test]
fn suspend_confirmation_falls_back_to_the_id_for_an_untitled_session() {
    let mut session = running_session();
    session.acp_session_title = None;
    session.title = String::new();
    let mut dashboard = dashboard_with_session(session);
    dashboard
        .session_details
        .get_mut("session-1")
        .unwrap()
        .current_turn_started_at = Some(1);
    chord(&mut dashboard, CommandId::SuspendSession);
    let suspend = drawn(&mut dashboard, 120, 40).join("\n");
    assert!(suspend.contains("Session: session-1"), "{suspend}");
}

#[test]
fn working_session_suspension_confirms_and_cancel_is_safe() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard
        .session_details
        .get_mut("session-1")
        .unwrap()
        .current_turn_started_at = Some(1);
    assert_eq!(
        chord(&mut dashboard, CommandId::SuspendSession),
        DashboardAction::None
    );
    assert!(
        matches!(&dashboard.mode, Mode::Confirm(dialog) if matches!(dialog.confirmation, crate::dialogs::Confirmation::SuspendSession { .. }))
    );
    // Enter initially activates Cancel.
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert!(matches!(dashboard.mode, Mode::Dashboard));
    chord(&mut dashboard, CommandId::SuspendSession);
    dashboard.handle_key(key(KeyCode::Right));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::Suspend {
            session_id: "session-1".into(),
            acknowledge_unpublished_work: true,
        }
    );
}
