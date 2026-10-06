use std::collections::BTreeMap;

use crossterm::event::{Event, KeyCode, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

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
    let mut dashboard = DashboardState::new(
        config(),
        State {
            last_subagent_policy: Default::default(),
            subagents: Default::default(),
            version: STATE_VERSION,
            sessions,
            mount_history: Default::default(),
            container_sizes: Default::default(),
        },
        BTreeMap::new(),
    );
    dashboard.select_active_session("session-0");
    dashboard
}

/// A session that becomes history releases the conversation and selection
/// together; only deliberate navigation selects a remaining row.
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
    assert_eq!(dashboard.selected_session_id(), None);
    assert_eq!(dashboard.current_session_id(), None);
    dashboard.handle_key(key(KeyCode::Down));
    assert_eq!(dashboard.selected_session_id(), Some("session-0"));
}

/// Launch campaign finding A-7: the cancel chord with nothing in flight
/// says so instead of doing nothing silently.
// Hard-won: 30a93a2: cancel with no pending operation was silent.
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
// Hard-won: 945a2da: suspended pinned panes retried an impossible attach.
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
// Hard-won: 81e032e: renaming a Starting session did nothing from the composer.
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
            // The pane is assigned immediately, before its chat attaches.
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

/// A launching session parks its conversation behind a composer the user can
/// edit; the draft, including a readline cursor edit, survives to the chat that opens.
#[test]
fn typing_during_a_launching_transition_edits_the_standby_draft() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let mut dashboard = dashboard_with_session(session);
    dashboard.begin_session_operation("session-1".into(), SessionOperationKind::Launching, None);
    dashboard.focus_prompt();

    for character in "hello world".chars() {
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Char(character))),
            DashboardAction::None
        );
    }
    dashboard.handle_key(alt_key('b'));
    dashboard.handle_key(key(KeyCode::Char('!')));

    assert_eq!(
        dashboard
            .standby_prompts
            .get("session-1")
            .map(|standby| standby.draft()),
        Some("hello !world".into())
    );
    assert_eq!(
        dashboard.take_standby_prompt_draft("session-1").as_deref(),
        Some("hello !world")
    );
    assert_eq!(dashboard.take_standby_prompt_draft("session-1"), None);
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

/// Recalling a queued prompt with Up takes it back from the daemon: the
/// preview goes, the composer holds the text, and Enter queues the edit once.
// Hard-won: 888bd1d: recalling queued text left the daemon step pending.
#[test]
fn recalling_a_queued_startup_prompt_withdraws_it_and_enter_requeues_the_edit() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let mut dashboard = dashboard_with_session(session);
    dashboard.begin_session_operation("session-1".into(), SessionOperationKind::Launching, None);
    dashboard.focus_prompt();
    for character in "first".chars() {
        dashboard.handle_key(key(KeyCode::Char(character)));
    }
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::QueueStartupPrompt {
            session_id: "session-1".into(),
            text: "first".into(),
        }
    );

    assert_eq!(
        dashboard.handle_key(key(KeyCode::Up)),
        DashboardAction::WithdrawStartupPrompt {
            session_id: "session-1".into(),
            text: "first".into(),
        }
    );
    let standby = dashboard.standby_prompts.get("session-1").expect("standby");
    assert!(standby.queued_prompt_texts().is_empty());
    assert_eq!(standby.draft(), "first");

    dashboard.handle_key(key(KeyCode::Char('!')));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::QueueStartupPrompt {
            session_id: "session-1".into(),
            text: "first!".into(),
        }
    );
    let standby = dashboard.standby_prompts.get("session-1").expect("standby");
    assert_eq!(standby.queued_prompt_texts(), vec!["first!".to_owned()]);
}

/// The daemon says the prompt had already gone out: the composer drops its
/// unchanged copy so Enter cannot send it twice, and keeps an edited one.
#[test]
fn a_recalled_prompt_that_was_already_sent_is_not_kept_as_an_unchanged_draft() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let mut dashboard = dashboard_with_session(session);
    dashboard.begin_session_operation("session-1".into(), SessionOperationKind::Launching, None);
    dashboard.focus_prompt();
    dashboard.handle_paste("sent");

    assert!(dashboard.standby_prompt_was_sent("session-1", "sent"));
    assert_eq!(dashboard.standby_prompts["session-1"].draft(), "");

    dashboard.handle_paste("sent and edited");
    assert!(!dashboard.standby_prompt_was_sent("session-1", "sent"));
    assert_eq!(
        dashboard.standby_prompts["session-1"].draft(),
        "sent and edited"
    );
}

/// If the daemon cannot be asked, the prompt may still go out, so it shows as
/// queued again rather than leaving the person to think the queue is empty.
#[test]
fn an_unconfirmed_withdrawal_shows_the_prompt_as_queued_again() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let mut dashboard = dashboard_with_session(session);
    dashboard.begin_session_operation("session-1".into(), SessionOperationKind::Launching, None);
    dashboard.focus_prompt();
    dashboard.handle_paste("maybe");
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Up));

    dashboard.standby_prompt_withdrawal_unconfirmed("session-1", "maybe");

    let standby = dashboard.standby_prompts.get("session-1").expect("standby");
    assert_eq!(standby.queued_prompt_texts(), vec!["maybe".to_owned()]);
    assert_eq!(standby.draft(), "");
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

    let mut other = running_session();
    other.id = "session-other".into();
    dashboard.state.sessions.insert(other.id.clone(), other);
    dashboard.select_active_session("session-other");

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

// Hard-won: c4fbb1a6: a missing session bundle blocked opening the affected session without repair guidance.
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
// Hard-won: 7c56a0f: incidental keys hid a background notice before it rendered.
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
    dashboard.state.sessions.get_mut("second").unwrap().state = SessionState::Running;
    assert_eq!(ids(&dashboard), ["second", "first"]);
    dashboard
        .state
        .sessions
        .get_mut("second")
        .unwrap()
        .workspace_id = "elsewhere".into();
    assert_eq!(ids(&dashboard), ["first"]);
}

// Hard-won: a74134f: workspace moves left a selected session in both tabs.
#[test]
fn a_feed_record_with_a_new_workspace_moves_the_row_between_tabs() {
    let mut dashboard = dashboard_with_attention_mix();
    let ids = |dashboard: &DashboardState| {
        dashboard
            .ordered_sessions()
            .iter()
            .map(|session| session.id.clone())
            .collect::<Vec<_>>()
    };
    dashboard.set_active_workspace(Some("default".into()));
    // The session is open in a pane, so it is the selected one: the list keeps
    // the selected row visible, which must not outlive its workspace.
    dashboard.select_active_session("quiet");
    assert_eq!(dashboard.selected_session_id(), Some("quiet"));
    assert!(ids(&dashboard).contains(&"quiet".to_owned()));
    let mut next = dashboard.state.clone();
    next.sessions.get_mut("quiet").unwrap().workspace_id = "other".into();
    dashboard.set_state(next);
    assert_eq!(dashboard.selected_session_id(), None);
    assert!(!ids(&dashboard).contains(&"quiet".to_owned()));
    dashboard.set_active_workspace(Some("other".into()));
    assert!(ids(&dashboard).contains(&"quiet".to_owned()));
    dashboard.set_active_workspace(Some("default".into()));
    assert!(!ids(&dashboard).contains(&"quiet".to_owned()));
    assert_eq!(dashboard.selected_session_id(), None);
}

/// A stopped Mjolnir sub-agent has no worker to attach to. Found live: its
/// conversation sat on "Bringing your conversation into focus" until the
/// attach timed out, and the work it did could not be read. It opens as a
/// read-only view of its stored transcript instead.
// Hard-won: bc0cee0: finished child transcripts could not be opened or read.
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
            last_subagent_policy: Default::default(),
            subagents: [(child.id.clone(), relation)].into_iter().collect(),
            version: STATE_VERSION,
            sessions: [
                (parent.id.clone(), parent.clone()),
                (child.id.clone(), child.clone()),
            ]
            .into_iter()
            .collect(),
            mount_history: Default::default(),
            container_sizes: Default::default(),
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
                let pane = dashboard.browse_pane();
                dashboard.render_stopped_subagent(frame, pane, "child-session", transcript, prompt);
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
// Hard-won: 4a99630: runtime snapshots omitted new child relations from workspace filtering.
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
            last_subagent_policy: Default::default(),
            subagents: [(first_child.id.clone(), first_relation.clone())]
                .into_iter()
                .collect(),
            version: STATE_VERSION,
            sessions: [
                (parent.id.clone(), parent.clone()),
                (first_child.id.clone(), first_child.clone()),
            ]
            .into_iter()
            .collect(),
            mount_history: Default::default(),
            container_sizes: Default::default(),
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
        last_subagent_policy: Default::default(),
        subagents: [
            (first_child.id.clone(), first_relation),
            (second_child.id.clone(), second_relation),
        ]
        .into_iter()
        .collect(),
        version: STATE_VERSION,
        sessions: [
            (parent.id.clone(), parent.clone()),
            (first_child.id.clone(), first_child.clone()),
            (second_child.id.clone(), second_child.clone()),
        ]
        .into_iter()
        .collect(),
        mount_history: Default::default(),
        container_sizes: Default::default(),
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

// Hard-won: 19a4148: mark-all-read cleared unread state in other workspaces.
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

// Hard-won: 233796e: wheel input scrolled the focused pane instead of the hovered pane.
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
            last_subagent_policy: Default::default(),
            subagents: Default::default(),
            version: STATE_VERSION,
            sessions,
            mount_history: Default::default(),
            container_sizes: Default::default(),
        },
        BTreeMap::new(),
    );
    dashboard.select_active_session("session-0");
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
            last_subagent_policy: Default::default(),
            subagents: Default::default(),
            version: STATE_VERSION,
            sessions,
            mount_history: Default::default(),
            container_sizes: Default::default(),
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
            last_subagent_policy: Default::default(),
            subagents: Default::default(),
            version: STATE_VERSION,
            sessions: [(other.id.clone(), other)].into_iter().collect(),
            mount_history: Default::default(),
            container_sizes: Default::default(),
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
// Hard-won: 4d523f1: launch completion replaced the user’s newer selection and focus.
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
// Hard-won: 0d95dcf: refreshing during launch lost the session the user had started.
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
    assert_eq!(dashboard.selected_session_id(), None);
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
// Hard-won: 0d95dcf: suspending a pinned session triggered a needless 15-second attach.
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
    assert_eq!(dashboard.navigation.layout().pane_count(), 1);
    assert_eq!(
        dashboard.notice().as_deref(),
        Some(crate::SPLIT_REFUSED_NOTICE)
    );
}

/// Launch finding D-2: the refusal notice describes a failed split, so a
/// later split that succeeds must take it off the bar.
// Hard-won: 34e23bc: a successful split left an obsolete refusal notice visible.
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

/// Three live sessions in the default workspace and one in `other`: `asks`
/// is waiting on a question, `done` has an unread answer, `quiet` is idle and
/// read, and `remote` (in `other`) is also waiting on a question.
fn dashboard_with_attention_mix() -> DashboardState {
    let mut sessions = mj_core::snapshot_map::SnapshotMap::new();
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
            last_subagent_policy: Default::default(),
            subagents: Default::default(),
            version: STATE_VERSION,
            sessions,
            mount_history: Default::default(),
            container_sizes: Default::default(),
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

fn append_attention_state(
    output: &mut String,
    label: &str,
    dashboard: &mut DashboardState,
    width: u16,
    height: u16,
) -> Vec<String> {
    use std::fmt::Write as _;

    let lines = drawn(dashboard, width, height);
    if !output.is_empty() {
        output.push('\n');
    }
    writeln!(output, "=== {label} ({width}x{height}) ===").expect("write state header");
    output.push_str(&lines.join("\n"));
    output.push('\n');
    lines
}

fn attention_values(dashboard: &DashboardState) -> String {
    let levels = ["asks", "remote", "done", "quiet", "missing"]
        .map(|id| format!("{id}={:?}", dashboard.attention_level(id)))
        .join(", ");
    let queue = dashboard
        .attention_queue()
        .into_iter()
        .map(|entry| format!("{}:{:?}", entry.session_id, entry.level))
        .collect::<Vec<_>>()
        .join(", ");
    format!("attention levels: {levels}; queue: [{queue}]")
}

fn rendered_workspace_badge(
    dashboard: &mut DashboardState,
    workspace_id: &str,
    width: u16,
    height: u16,
) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal
        .draw(|frame| render(frame, dashboard))
        .expect("draw workspace badge");
    let area = dashboard
        .workspace_tab_areas
        .iter()
        .find(|(id, _)| id == workspace_id)
        .map(|(_, area)| *area)
        .expect("workspace tab");
    let buffer = terminal.backend().buffer();
    let marker = &buffer[(area.right() - 3, area.y)];
    let count = &buffer[(area.right() - 2, area.y)];
    format!(
        "workspace badge cells: {}{}; marker fg={:?}; failure color={}",
        marker.symbol(),
        count.symbol(),
        marker.fg,
        marker.fg == mj_chat::theme::palette().session_error
    )
}

/// The Sessions order and the attention levels are derived once per change
/// of what they come from, however often a frame reads them, and each change
/// is reflected in the next read.
#[test]
fn session_order_and_attention_are_derived_once_per_change_and_follow_it() {
    let mut dashboard = dashboard_with_attention_mix();
    dashboard.config.advanced.session_order = mj_core::config::SessionOrder::Priority;
    dashboard.set_active_workspace(Some("default".into()));
    dashboard.select_active_session("done");
    let ordered = |dashboard: &DashboardState| {
        dashboard
            .ordered_sessions()
            .iter()
            .map(|session| session.id.clone())
            .collect::<Vec<_>>()
    };
    drawn(&mut dashboard, 160, 40);
    assert_eq!(ordered(&dashboard), ["asks", "done", "quiet"]);

    let settled = dashboard.session_view_derivations();
    for _ in 0..3 {
        drawn(&mut dashboard, 160, 40);
        dashboard.notification_events(0);
    }
    assert_eq!(
        dashboard.session_view_derivations(),
        settled,
        "frames with nothing changed derive nothing"
    );

    // A question makes `quiet` waiting too. It was created first, so it now
    // leads the sessions at that level.
    dashboard
        .session_details
        .get_mut("quiet")
        .unwrap()
        .pending_elicitations = vec![question("quiet")];
    drawn(&mut dashboard, 160, 40);
    drawn(&mut dashboard, 160, 40);
    assert_eq!(dashboard.attention_level("quiet"), AttentionLevel::Waiting);
    assert_eq!(ordered(&dashboard), ["quiet", "asks", "done"]);
    assert_eq!(
        dashboard.session_view_derivations(),
        (settled.0 + 1, settled.1 + 1),
        "one change, one derivation"
    );

    // A filter narrows the list to what matches, and says how much it hides.
    dashboard.focus_sessions();
    dashboard.handle_key(key(KeyCode::Char('/')));
    for c in "done".chars() {
        dashboard.handle_key(key(KeyCode::Char(c)));
    }
    dashboard.handle_key(key(KeyCode::Enter));
    assert_eq!(ordered(&dashboard), ["done"]);
    assert_eq!(dashboard.sessions_hidden_count(), 2);

    // Moving the selection to a hidden session keeps that session listed.
    dashboard.select_active_session("asks");
    assert_eq!(ordered(&dashboard), ["asks", "done"]);
    assert_eq!(dashboard.sessions_hidden_count(), 1);

    *dashboard.sessions_filter = None;
    assert_eq!(ordered(&dashboard), ["quiet", "asks", "done"]);
    assert_eq!(dashboard.sessions_hidden_count(), 0);
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

// Hard-won: bed9102: terminal failures inflated badges and queued an unopenable session.
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

// Hard-won: 1b2ef72: short names and wide graphemes clipped the workspace badge.
#[test]
fn short_workspace_names_keep_their_attention_badges() {
    for name in ["M", "MJ", "界", "e\u{301}"] {
        let mut dashboard = dashboard_with_attention_mix();
        dashboard.set_workspace_names(BTreeMap::from([
            ("default".into(), name.into()),
            ("other".into(), "Other".into()),
        ]));
        dashboard.set_active_workspace(Some("default".into()));
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .unwrap();
        let (_, area) = dashboard
            .workspace_tab_areas
            .iter()
            .find(|(id, _)| id == "default")
            .expect("workspace tab");
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(area.right() - 3, area.y)].symbol(), "!", "{name}");
        assert_eq!(buffer[(area.right() - 2, area.y)].symbol(), "1", "{name}");
    }
}

#[test]
fn golden_attention_navigation() {
    use std::fmt::Write as _;

    let mut output = String::new();

    let mut ranked = dashboard_with_attention_mix();
    ranked.set_active_workspace(Some("default".into()));
    append_attention_state(
        &mut output,
        "question, unread, idle, and absent levels",
        &mut ranked,
        140,
        40,
    );
    writeln!(output, "{}", attention_values(&ranked)).expect("write attention levels");

    let mut urgent = dashboard_with_attention_mix();
    urgent.set_active_workspace(Some("default".into()));
    urgent.set_session_reviews([mj_client::review::RuntimeReviewView {
        session_id: "asks".into(),
        questions: Vec::new(),
        phase: mj_core::review::driver::TurnReviewPhase::Verdict(
            mj_core::review::verdict::ReviewVerdict::Failed {
                reason: "the reviewer never answered".into(),
            },
        ),
        roles: Vec::new(),
        status: "the review failed".into(),
        verdict: None,
    }]);
    urgent.set_session_connectivity("remote", false);
    append_attention_state(
        &mut output,
        "failure, unreachable worker, and question",
        &mut urgent,
        140,
        40,
    );
    writeln!(output, "{}", attention_values(&urgent)).expect("write urgent levels");
    writeln!(
        output,
        "rank relation: Failed>Unreachable={}, Unreachable>Waiting={}",
        AttentionLevel::Failed > AttentionLevel::Unreachable,
        AttentionLevel::Unreachable > AttentionLevel::Waiting
    )
    .expect("write attention order");

    let mut badges = dashboard_with_attention_mix();
    badges.set_active_workspace(Some("default".into()));
    append_attention_state(
        &mut output,
        "waiting badge counts only its level",
        &mut badges,
        120,
        40,
    );
    writeln!(
        output,
        "workspace summary: {:?}",
        badges.workspace_attention_summary("default")
    )
    .expect("write waiting summary");
    writeln!(
        output,
        "{}",
        rendered_workspace_badge(&mut badges, "default", 120, 40)
    )
    .expect("write waiting badge style");
    badges.set_session_connectivity("done", false);
    append_attention_state(
        &mut output,
        "unreachable badge replaces unread",
        &mut badges,
        120,
        40,
    );
    writeln!(
        output,
        "workspace summary: {:?}",
        badges.workspace_attention_summary("default")
    )
    .expect("write unreachable summary");
    writeln!(
        output,
        "{}",
        rendered_workspace_badge(&mut badges, "default", 120, 40)
    )
    .expect("write unreachable badge style");
    badges.state.sessions.get_mut("quiet").unwrap().state = SessionState::Error;
    append_attention_state(
        &mut output,
        "failure badge replaces lower levels",
        &mut badges,
        120,
        40,
    );
    writeln!(
        output,
        "workspace summary: {:?}",
        badges.workspace_attention_summary("default")
    )
    .expect("write failure summary");
    writeln!(
        output,
        "{}",
        rendered_workspace_badge(&mut badges, "default", 120, 40)
    )
    .expect("write failure badge style");
    let mut quiet = dashboard_with_session(running_session());
    append_attention_state(
        &mut output,
        "quiet workspace has no attention badge",
        &mut quiet,
        120,
        40,
    );
    writeln!(
        output,
        "workspace summary: {:?}; badge: absent",
        quiet.workspace_attention_summary("default")
    )
    .expect("write quiet summary");

    let mut next = dashboard_with_attention_mix();
    next.set_active_workspace(Some("default".into()));
    next.session_details
        .get_mut("asks")
        .unwrap()
        .last_activity_at_ms = Some(20);
    next.session_details
        .get_mut("remote")
        .unwrap()
        .last_activity_at_ms = Some(10);
    next.select_active_session("quiet");
    let action = chord(&mut next, CommandId::NextAttention);
    append_attention_state(
        &mut output,
        "next attention opens the first waiting session",
        &mut next,
        140,
        40,
    );
    writeln!(
        output,
        "action: {action:?}; selected: {:?}; prompt focused: {}",
        next.selected_session_id(),
        next.prompt_has_focus()
    )
    .expect("write first attention action");
    let action = chord(&mut next, CommandId::NextAttention);
    append_attention_state(
        &mut output,
        "next attention reaches another workspace",
        &mut next,
        140,
        40,
    );
    writeln!(output, "action: {action:?}").expect("write workspace action");
    next.set_active_workspace(Some("other".into()));
    append_attention_state(
        &mut output,
        "other workspace selection is applied",
        &mut next,
        140,
        40,
    );
    writeln!(output, "selected: {:?}", next.selected_session_id())
        .expect("write workspace selection");
    let action = chord(&mut next, CommandId::PreviousAttention);
    append_attention_state(
        &mut output,
        "previous attention returns to the default workspace",
        &mut next,
        140,
        40,
    );
    writeln!(output, "action: {action:?}").expect("write previous action");
    next.set_active_workspace(Some("default".into()));
    append_attention_state(
        &mut output,
        "default workspace selection is restored",
        &mut next,
        140,
        40,
    );
    writeln!(output, "selected: {:?}", next.selected_session_id())
        .expect("write restored selection");
    next.select_active_session("done");
    let action = chord(&mut next, CommandId::NextAttention);
    append_attention_state(
        &mut output,
        "next attention wraps to the first waiting session",
        &mut next,
        140,
        40,
    );
    writeln!(
        output,
        "action: {action:?}; selected: {:?}",
        next.selected_session_id()
    )
    .expect("write wrapped action");

    let mut folded = dashboard_with_attention_mix();
    folded.set_active_workspace(Some("default".into()));
    let before_fold = append_attention_state(
        &mut output,
        "project rows before folding",
        &mut folded,
        140,
        40,
    );
    let asks_heading = point(&before_fold, "asks");
    folded.handle_mouse(mouse_at(
        MouseEventKind::Down(MouseButton::Left),
        asks_heading,
    ));
    folded.handle_mouse(mouse_at(
        MouseEventKind::Up(MouseButton::Left),
        asks_heading,
    ));
    append_attention_state(&mut output, "waiting project folded", &mut folded, 140, 40);
    folded
        .session_details
        .get_mut("asks")
        .unwrap()
        .last_activity_at_ms = Some(20);
    folded.select_active_session("quiet");
    let action = chord(&mut folded, CommandId::NextAttention);
    append_attention_state(
        &mut output,
        "next attention unfolds and opens the waiting project",
        &mut folded,
        140,
        40,
    );
    writeln!(
        output,
        "action: {action:?}; folded projects: {:?}",
        folded.collapsed_project_keys
    )
    .expect("write unfold action");
    for id in ["asks", "remote"] {
        folded
            .session_details
            .get_mut(id)
            .unwrap()
            .pending_elicitations
            .clear();
    }
    folded
        .session_details
        .get_mut("done")
        .unwrap()
        .unread_agent_messages = 0;
    let action = chord(&mut folded, CommandId::NextAttention);
    append_attention_state(
        &mut output,
        "empty attention queue reports its notice",
        &mut folded,
        140,
        40,
    );
    writeln!(
        output,
        "action: {action:?}; notice: {:?}",
        folded.notices.current()
    )
    .expect("write empty queue");

    let mut footer = dashboard_with_attention_mix();
    footer.set_active_workspace(Some("default".into()));
    let lines = append_attention_state(
        &mut output,
        "footer advertises next attention while waiting",
        &mut footer,
        200,
        40,
    );
    writeln!(output, "footer: {}", lines.last().unwrap()).expect("write waiting footer");
    let mut quiet_footer = dashboard_with_session(running_session());
    let lines = append_attention_state(
        &mut output,
        "footer omits next attention while quiet",
        &mut quiet_footer,
        200,
        40,
    );
    writeln!(output, "footer: {}", lines.last().unwrap()).expect("write quiet footer");

    let mut tabs = dashboard_with_attention_mix();
    tabs.set_active_workspace(Some("default".into()));
    let first = append_attention_state(
        &mut output,
        "workspace tabs show their urgent badges",
        &mut tabs,
        120,
        40,
    );
    let tab_row = first
        .iter()
        .find(|line| line.contains("Default") && line.contains("Other"))
        .expect("workspace tabs");
    writeln!(output, "workspace tabs: {tab_row}").expect("write tab badges");
    let done_heading = point(&first, "done");
    tabs.handle_mouse(mouse_at(
        MouseEventKind::Down(MouseButton::Left),
        done_heading,
    ));
    tabs.handle_mouse(mouse_at(
        MouseEventKind::Up(MouseButton::Left),
        done_heading,
    ));
    let folded_lines = append_attention_state(
        &mut output,
        "folded project heading carries unread badge",
        &mut tabs,
        120,
        40,
    );
    let heading = folded_lines
        .iter()
        .find(|line| line.contains("done ✓1"))
        .expect("unread badge on folded heading");
    writeln!(output, "folded heading: {heading}").expect("write folded heading");

    let mut priority = dashboard_with_attention_mix();
    priority.set_active_workspace(Some("default".into()));
    let mut config = priority.config.clone();
    config.advanced.session_order = mj_core::config::SessionOrder::Priority;
    priority.set_config(config);
    append_attention_state(
        &mut output,
        "priority order lists attention before idle",
        &mut priority,
        140,
        40,
    );
    let ids = priority
        .ordered_sessions()
        .into_iter()
        .map(|session| session.id.clone())
        .collect::<Vec<_>>();
    let no_headings = priority
        .sessions_rows()
        .iter()
        .all(|row| matches!(row, SessionsRow::Session { .. }));
    writeln!(
        output,
        "priority session ids: {ids:?}; project headings: {}",
        !no_headings
    )
    .expect("write priority order");
    priority.focus_sessions();
    priority.handle_key(key(KeyCode::Char('1')));
    append_attention_state(
        &mut output,
        "priority order reveals all rows",
        &mut priority,
        140,
        40,
    );
    writeln!(
        output,
        "notice: {:?}; collapsed projects: {:?}",
        priority.notices.current(),
        priority.collapsed_project_keys
    )
    .expect("write priority view");
    priority
        .session_details
        .get_mut("asks")
        .unwrap()
        .pending_elicitations
        .clear();
    let ids = priority
        .ordered_sessions()
        .into_iter()
        .map(|session| session.id.clone())
        .collect::<Vec<_>>();
    append_attention_state(
        &mut output,
        "answering the question lowers it below unread",
        &mut priority,
        140,
        40,
    );
    writeln!(output, "priority session ids after answer: {ids:?}")
        .expect("write updated priority order");

    mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), "attention-navigation", &output);
}

/// RCL-2 (2026-09-29): with the Sessions pane minimized, the title still says
/// a filter is on.
// Hard-won: daa8f51: minimized attention titles dropped the active filter indicator.
#[test]
fn a_minimized_sessions_pane_title_still_shows_the_filter_chip() {
    let mut dashboard = dashboard_with_attention_mix();
    dashboard.set_active_workspace(Some("default".into()));
    dashboard.focus_sessions();
    dashboard.handle_key(key(KeyCode::Char('/')));
    for c in "ask".chars() {
        dashboard.handle_key(key(KeyCode::Char(c)));
    }
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.set_pane_size(SupportPane::Sessions, PaneSize::Minimized);
    assert!(dashboard.sessions_filter.is_some());

    let lines = drawn(&mut dashboard, 80, 40);
    let pane = dashboard.pane_areas.expect("pane areas")[0];
    let title = &lines[usize::from(pane.y)];
    let title = title
        .chars()
        .skip(usize::from(pane.x))
        .take(usize::from(pane.width))
        .collect::<String>();
    assert!(title.contains('×'), "{title:?}");
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
// Hard-won: cf352ae: the done filter read its unread answer and then hid that row.
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
        ["alpha", "beta"]
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
// Hard-won: 6de42d8: an empty filtered list stopped accepting its own clear keys.
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

fn git_status_fixture() -> mj_core::local_git::SessionGitStatus {
    mj_core::local_git::parse_git_status(
        std::path::PathBuf::from("/work"),
        "feature/x",
        Some("2\t1"),
        "12\t3\tsrc/main.rs\n",
        " M src/main.rs\n?? notes.md\n",
    )
}

/// Every session created with a managed worktree gets a `mj/<32 hex>` branch,
/// which is wider than the sidebar, so dropping the whole marker hid the
/// feature at the widths people use. The middle of the name is what nobody
/// reads: elide it and the marker keeps its ahead, behind, and changed counts.
// Hard-won: ddb00ad: long generated branch names hid all checkout status at normal widths.
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
// Hard-won: 3ea7e85: the per-pass limit permanently starved sessions beyond the first batch.
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

// Hard-won: b9e5e3b: failed targets were still attached until the open attempt could not finish.
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

// Hard-won: b9e5e3b: long failure notices lost their diagnostic tail at terminal width.
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
// Hard-won: 936d6ee: finished native children appeared empty and could not be opened.
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
    // The pane's title row names the child too; the row is in Sessions.
    let row = lines
        .iter()
        .position(|line| line.contains("Review calc · completed") && !line.contains("Conversation"))
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

/// Launch finding R12-3 (also R11 tmux/012): a native child's pane header
/// read "Browse | Conversation d · availability unknown". The pane chrome
/// draws its label over the left of the title row and its chips over the
/// right, and the native pane's own title started under the label, which
/// covered " Review calc · complete". The title starts after the label, names
/// the child first, and drops status words from the end when the row is
/// short. At 80 columns the pane is 42 wide and the chrome leaves 7 columns,
/// so the name itself is cut.
// Hard-won: 90ef5fd: pane chrome obscured most of a native child’s title.
#[test]
fn a_native_child_pane_header_names_the_child_first() {
    let (mut dashboard, parent_id, id) = dashboard_with_finished_native_child();
    dashboard.open_subagent_workspace(parent_id);
    let browse = dashboard.browse_pane();
    dashboard.set_pane_session(browse, Some(&id));
    for (width, title) in [
        (140, "Review calc · completed · availability unknown "),
        (100, "Review calc · completed "),
        (90, "Review calc "),
        (80, "Rev…"),
    ] {
        let header = drawn(&mut dashboard, width, 40)
            .into_iter()
            .find(|line| line.contains("Browse | Conversation"))
            .expect("the pane's title row");
        assert!(
            header.contains(&format!("Browse | Conversation  {title}─")),
            "{width} columns: {header}"
        );
    }
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

/// Launch finding R11-1: a Claude sub-agent sat on a permission question in
/// the Sub-agents view while its parent's row read only "Working" with no
/// attention mark, so nobody knew to look. A child's question marks its parent
/// the way the parent's own question would: the row's symbol, the attention
/// queue and a notification, and the row says whose question it is.
// Hard-won: 91244ea: a child permission question left its parent without an attention mark.
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

/// A suspend stops the parent's sub-agents without a checkpoint of their own.
/// It asks first only when one of them is still at its task, and says, by
/// its listed title, that suspending stops it (R15-4); an idle child has
/// handed back and stops silently.
#[test]
fn parent_suspension_warns_only_about_subagents_still_at_their_task() {
    let (mut dashboard, parent) = crate::test_support::dashboard_with_one_subagent();
    // A raw project has no clone whose publication needs confirming.
    dashboard
        .state
        .sessions
        .get_mut(&parent)
        .unwrap()
        .project_directory = Some("/srv/project".into());
    dashboard
        .state
        .sessions
        .get_mut("child-session")
        .unwrap()
        .session_title_override = Some("sleep-100".into());
    dashboard.focus_sessions();
    assert_eq!(dashboard.attention_level(&parent), AttentionLevel::Idle);
    assert_eq!(
        dashboard.dispatch_command(CommandId::SuspendSession),
        DashboardAction::Suspend {
            session_id: parent.clone(),
            acknowledge_unpublished_work: false,
        }
    );

    dashboard
        .session_details
        .get_mut("child-session")
        .unwrap()
        .current_turn_started_at = Some(1);
    assert_eq!(dashboard.attention_level(&parent), AttentionLevel::Idle);
    assert_eq!(
        dashboard.dispatch_command(CommandId::SuspendSession),
        DashboardAction::None
    );
    assert!(matches!(
        &dashboard.mode,
        Mode::Confirm(dialog) if matches!(
            dialog.confirmation,
            crate::dialogs::Confirmation::SuspendSession {
                interrupting: false,
                ..
            }
        )
    ));
    let dialog = drawn(&mut dashboard, 120, 40).join(
        "
",
    );
    assert!(
        dialog.contains("Sub-agent \"sleep-100\" has not handed back; suspending stops it."),
        "{dialog}"
    );
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Esc)),
        DashboardAction::None
    );
    assert!(matches!(dashboard.mode, Mode::Dashboard));
}

/// Launch finding B-3: the row menu is titled with the session's name, so
/// the Suspend confirmation and the Rename dialog must name it the same way
/// instead of by its id.
// Hard-won: 78e2c60: session dialogs showed opaque ids instead of the known title.
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
// Hard-won: f639b25: dialogs discarded the created title and showed an opaque id.
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

/// The records a snapshot brings once the parent's suspend has stopped its
/// one sub-agent: the parent is closing and the child is gone.
fn stopped_by_the_parents_suspend(
    dashboard: &DashboardState,
    parent: &str,
) -> mj_core::state::State {
    let mut state = dashboard.state.clone();
    state.sessions.get_mut(parent).unwrap().state = SessionState::Closing;
    state.sessions.remove("child-session");
    state.subagents.remove("child-session");
    state
}

/// R15-1: a suspend stopped the sub-agent whose conversation was open in its
/// parent's Sub-agents view. The view fell to "No sessions yet" and the
/// footer said "Could not save draft and read status for <child>: unknown
/// session". It goes back to the parent's scope with the parent selected,
/// and says what happened.
// Hard-won: 6e2e36c: suspending a parent lost the open child’s draft and read state.
#[test]
fn a_suspend_that_stops_the_open_sub_agent_goes_back_to_its_parent() {
    let (mut dashboard, parent) = crate::test_support::dashboard_with_one_subagent();
    dashboard.open_subagent_workspace(parent.clone());
    dashboard.set_current_session(Some("child-session"));
    assert_eq!(dashboard.selected_session_id(), Some("child-session"));
    let title = dashboard.state.sessions["child-session"]
        .listed_title()
        .to_owned();

    dashboard.set_state(stopped_by_the_parents_suspend(&dashboard, &parent));

    assert_eq!(dashboard.subagent_parent_id(), None);
    assert_eq!(dashboard.selected_session_id(), Some(parent.as_str()));
    assert_eq!(
        dashboard.notice(),
        Some(format!("Sub-agent \"{title}\" was stopped by the suspend"))
    );
    let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
    terminal
        .draw(|frame| {
            let layout = crate::render::SessionsLayout::new(&dashboard, frame.area().width - 2);
            crate::render::render_sessions(frame, frame.area(), &dashboard, &layout);
        })
        .unwrap();
    let rows = terminal.backend().to_string();
    assert!(!rows.contains("No sessions yet"), "{rows}");
}

/// The host lets the stopped sub-agent's conversation go without saving a
/// draft for a session that no longer exists, and opens the parent's.
// Hard-won: 6e2e36c: the host did not learn which child conversations the suspend retired.
#[test]
fn the_host_learns_which_conversations_a_suspend_took_away() {
    let (mut dashboard, parent) = crate::test_support::dashboard_with_one_subagent();
    dashboard.open_subagent_workspace(parent.clone());

    dashboard.set_state(stopped_by_the_parents_suspend(&dashboard, &parent));

    assert_eq!(
        dashboard.take_stopped_by_suspend(),
        StoppedBySuspend {
            sessions: vec!["child-session".into()],
            reopen: Some(parent.clone()),
        }
    );
    assert_eq!(
        dashboard.take_stopped_by_suspend(),
        StoppedBySuspend::default()
    );
}

/// The feed carries only live sessions, so a parent whose suspend finished
/// can leave in the same frame as the sub-agent it stopped. That is still the
/// suspend's doing, and there is no parent conversation left to reopen.
#[test]
fn a_parent_and_sub_agent_leaving_together_after_a_suspend_is_still_reported() {
    let (mut dashboard, parent) = crate::test_support::dashboard_with_one_subagent();
    dashboard.open_subagent_workspace(parent.clone());
    dashboard.set_current_session(Some("child-session"));
    let mut closing = dashboard.state.clone();
    closing.sessions.get_mut(&parent).unwrap().state = SessionState::Closing;
    dashboard.set_state(closing);
    assert_eq!(
        dashboard.take_stopped_by_suspend(),
        StoppedBySuspend::default()
    );

    let mut gone = dashboard.state.clone();
    gone.sessions.remove(&parent);
    gone.sessions.remove("child-session");
    gone.subagents.remove("child-session");
    dashboard.set_state(gone);

    assert_eq!(
        dashboard.take_stopped_by_suspend(),
        StoppedBySuspend {
            sessions: vec!["child-session".into()],
            reopen: None,
        }
    );
    assert!(
        dashboard
            .notice()
            .is_some_and(|notice| notice.contains("was stopped by the suspend")),
        "{:?}",
        dashboard.notice()
    );
}

/// A suspended session leaves the feed instead of staying as a stopped
/// record. A pinned pane that showed it still says why it emptied.
#[test]
fn a_pinned_session_that_leaves_the_feed_by_suspending_says_why_its_pane_emptied() {
    let mut dashboard = dashboard_with_session(running_session());
    let mut pinned = running_session();
    pinned.id = "session-2".into();
    let mut state = dashboard.state.clone();
    state.sessions.insert(pinned.id.clone(), pinned);
    dashboard.set_state(state);
    let pane = dashboard.browse_pane();
    dashboard
        .split_focused_pane(ratatui::layout::Direction::Horizontal, None)
        .unwrap();
    dashboard.set_pane_session(pane, Some("session-2"));
    assert_ne!(pane, dashboard.browse_pane());
    dashboard.begin_session_operation_at_with_id(
        "session-2".into(),
        SessionOperationKind::Suspending,
        None,
        0,
        None,
        false,
    );

    let mut next = dashboard.state.clone();
    next.sessions.remove("session-2");
    dashboard.set_state(next);
    dashboard.finish_session_operation("session-2");

    assert_eq!(dashboard.pane_session(pane), None);
    assert!(
        dashboard
            .notice()
            .is_some_and(|notice| notice.contains("is suspended, so it was unpinned from its pane")),
        "{:?}",
        dashboard.notice()
    );
}

/// A sub-agent that leaves while its parent runs on, as a destroy removes
/// one, was not stopped by a suspend: the view and the footer stay as they
/// were.
#[test]
fn a_sub_agent_removed_while_its_parent_runs_is_not_reported_as_suspended() {
    let (mut dashboard, parent) = crate::test_support::dashboard_with_one_subagent();
    dashboard.open_subagent_workspace(parent.clone());
    let mut state = dashboard.state.clone();
    state.sessions.remove("child-session");
    state.subagents.remove("child-session");

    dashboard.set_state(state);

    assert_eq!(dashboard.subagent_parent_id(), Some(parent.as_str()));
    assert_eq!(dashboard.notice(), None);
    assert_eq!(
        dashboard.take_stopped_by_suspend(),
        StoppedBySuspend::default()
    );
}

/// R15-2: a sub-agent that its parent's suspend is stopping reads
/// "Stopping" in its row and in its conversation's header, as the suspend
/// dialog and the docs say. Before, both said "Destroying". A destroy a
/// person asked for still says "Destroying".
// Hard-won: 6bfe27a: parent-driven child suspension was mislabeled as destruction.
#[test]
fn a_sub_agent_stopped_by_its_parents_suspend_reads_stopping() {
    let (mut dashboard, parent) = crate::test_support::dashboard_with_one_subagent();
    dashboard.open_subagent_workspace(parent);
    dashboard.set_current_session(Some("child-session"));

    dashboard.begin_session_operation("child-session".into(), SessionOperationKind::Stopping, None);
    let screen = drawn(&mut dashboard, 120, 40).join("\n");
    assert!(screen.contains("Transition · Stopping"), "{screen}");
    assert!(!screen.contains("Destroying"), "{screen}");

    dashboard.finish_session_operation("child-session");
    dashboard.begin_session_operation(
        "child-session".into(),
        SessionOperationKind::Destroying,
        None,
    );
    let screen = drawn(&mut dashboard, 120, 40).join("\n");
    assert!(screen.contains("Transition · Destroying"), "{screen}");
}

/// R15-4: the suspend confirmation names the sub-agents still at their task,
/// by listed title, up to three, and counts the rest.
// Hard-won: 1349c76: suspend confirmation counted active children without naming them.
#[test]
fn the_suspend_confirmation_names_three_working_sub_agents_and_counts_the_rest() {
    let (mut dashboard, parent) = crate::test_support::dashboard_with_one_subagent();
    dashboard
        .state
        .sessions
        .get_mut(&parent)
        .unwrap()
        .project_directory = Some("/srv/project".into());
    let template = dashboard.state.sessions["child-session"].clone();
    let relation = dashboard.state.subagents["child-session"].clone();
    let mut state = dashboard.state.clone();
    state.sessions.remove("child-session");
    state.subagents.remove("child-session");
    for (position, title) in ["Alpha", "Bravo", "Charlie", "Delta", "Echo"]
        .into_iter()
        .enumerate()
    {
        let mut child = template.clone();
        child.id = format!("child-{position}");
        child.session_title_override = Some(title.into());
        child.created_at = format!("2026-09-0{}T00:00:00Z", position + 1);
        let mut relation = relation.clone();
        relation.child_session_id = child.id.clone();
        state.subagents.insert(child.id.clone(), relation);
        state.sessions.insert(child.id.clone(), child);
    }
    dashboard.set_state(state);
    for position in 0..5 {
        dashboard
            .session_details
            .get_mut(&format!("child-{position}"))
            .unwrap()
            .current_turn_started_at = Some(1);
    }
    dashboard.focus_sessions();
    dashboard.select_active_session(&parent);

    assert_eq!(
        dashboard.dispatch_command(CommandId::SuspendSession),
        DashboardAction::None
    );
    let dialog = drawn(&mut dashboard, 160, 40).join(" ");
    assert!(
        dialog.contains(
            "Sub-agents \"Alpha\", \"Bravo\", \"Charlie\" and 2 more have not handed back;"
        ),
        "{dialog}"
    );
}

#[test]
fn live_render_membership_and_record_updates_do_not_revisit_history() {
    for count in [100, 10_000, 100_000] {
        let mut active = running_session();
        active.id = "active".into();
        let mut dashboard = dashboard_with_session(active.clone());
        let mut state = dashboard.state.clone();
        for n in 0..count {
            let mut record = active.clone();
            record.id = format!("history-{n}");
            record.state = SessionState::Stopped;
            state.sessions.insert(record.id.clone(), record);
        }
        dashboard.set_state(state.clone());
        assert_eq!(dashboard.ordered_sessions().len(), 1);
        dashboard.row_index.borrow_mut().visits = 0;
        state.sessions.get_mut("active").unwrap().title = "renamed".into();
        dashboard.set_state(state);
        assert_eq!(dashboard.ordered_sessions()[0].title, "renamed");
        dashboard.attention_queue();
        assert_eq!(
            dashboard.row_index.borrow().visits,
            1,
            "history size {count}"
        );
    }
}

#[test]
fn durable_updates_do_not_scan_unchanged_native_presentation_rows() {
    use mj_core::native_agent::*;
    for count in [100, 10_000, 100_000] {
        let parent = running_session();
        let mut dashboard = dashboard_with_session(parent.clone());
        let mut durable = dashboard.state.clone();
        dashboard.set_native_agents(
            (0..count)
                .map(|n| {
                    let agent = NativeAgent {
                        owner_session_id: parent.id.clone(),
                        session_id: format!("child-{n}"),
                        parent_session_id: None,
                        name: format!("Child {n}"),
                        task: String::new(),
                        capabilities: NativeAgentCapabilities::default(),
                        state: NativeAgentState::Completed,
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
                .collect(),
        );
        dashboard.set_state(durable.clone());
        dashboard.reconciliation_visits.set(0);
        durable.sessions.get_mut(&parent.id).unwrap().title = "Renamed".into();
        dashboard.set_state(durable);
        assert!(
            dashboard.reconciliation_visits.get() <= 2,
            "native history size {count}"
        );
        assert_eq!(dashboard.subagent_count_for(&parent.id), count);
        assert_eq!(dashboard.working_subagent_count_for(&parent.id), 0);
        assert!(
            dashboard.reconciliation_visits.get() <= 2,
            "working count visited native history {count}"
        );
    }
}

/// I2-7: the composer footer's working count stayed 0 for a child's whole
/// turn. The count must come from the facts the worker reports for the child,
/// which is all a dashboard holds for a child nobody has opened, and it must
/// agree with the child's Sessions row.
// Hard-won: 6b8110e: worker turn reports left the parent’s working count stale at zero.
#[test]
fn a_subagent_the_worker_reports_as_running_counts_as_working_for_its_parent() {
    let (mut dashboard, parent) = crate::test_support::dashboard_with_one_subagent();
    assert_eq!(dashboard.working_subagent_count_for(&parent), 0);

    dashboard.set_session_activity(
        "child-session",
        mj_client::usage_format::SessionActivity {
            execution: Some(mj_core::relay::RelayExecutionState::Running),
            state: Some(mj_core::activity::ActivityState::Turn {
                started_at_ms: Some(1_000),
                last_activity_at_ms: None,
            }),
            ..Default::default()
        },
    );

    assert_eq!(
        dashboard.attention_level("child-session"),
        AttentionLevel::Working,
        "the child's own row reads it as working"
    );
    assert_eq!(dashboard.working_subagent_count_for(&parent), 1);
}
