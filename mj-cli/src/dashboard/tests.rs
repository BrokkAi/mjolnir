#[test]
fn detaching_hidden_chat_preserves_unread_background_events() {
    assert_eq!(super::detach_read_frontier(false, 20, 12), 12);
    assert_eq!(super::detach_read_frontier(true, 20, 12), 20);
    assert_eq!(super::detach_read_frontier(true, 10, 12), 12);
}

use super::*;
use mj_chat::chat::{ActiveChat, Notices, SessionHeaderIdentity};
use mj_core::state::State;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Position;

#[test]
fn background_records_preserve_read_positions_without_resurrecting_sessions() {
    let mut old = mj_core::snapshot_map::SnapshotMap::from([(
        "session-1".to_owned(),
        live_session("session-1", "2026-09-19T00:00:00Z"),
    )]);
    let id = old.keys().next().unwrap().clone();
    old.get_mut(&id).unwrap().viewed_through_event_ordinal = 8;
    let mut incoming = old.clone();
    incoming.get_mut(&id).unwrap().viewed_through_event_ordinal = 2;
    read_receipts::preserve_read_positions(&mut incoming, &old);
    assert_eq!(incoming[&id].viewed_through_event_ordinal, 8);
    incoming.get_mut(&id).unwrap().viewed_through_event_ordinal = 10;
    read_receipts::preserve_read_positions(&mut incoming, &old);
    assert_eq!(incoming[&id].viewed_through_event_ordinal, 10);
    incoming.remove(&id);
    read_receipts::preserve_read_positions(&mut incoming, &old);
    assert!(!incoming.contains_key(&id));
}
use ratatui::style::Modifier;

fn mouse(kind: MouseEventKind, column: u16, row: u16) -> Event {
    Event::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    })
}

fn open_test_chat(session_id: &str) -> ActiveChat {
    open_test_chat_with_notices(session_id, Notices::default())
}

/// The same chat, but with a notice handle the caller keeps, so a test can
/// read what the conversation reported.
fn open_test_chat_with_notices(session_id: &str, notices: Notices) -> ActiveChat {
    let fixture = mj_client::session::replacement_session_test_fixture(session_id, 1);
    ActiveChat::open(
        fixture.stopped,
        "bundle-1",
        None,
        fixture.control,
        SessionHeaderIdentity::default(),
        String::new(),
        notices,
    )
}

/// A dashboard with profiles and a target, so the adaptive layout draws
/// its three panes with text in them.
fn populated_dashboard() -> DashboardState {
    let mut config = Config::default();
    for (id, kind) in [
        ("claude-1", mj_core::config::HarnessKind::Claude),
        ("codex-1", mj_core::config::HarnessKind::Codex),
    ] {
        config.profiles.insert(
            id.into(),
            mj_core::config::HarnessProfile {
                enabled: true,
                context_window_bytes: None,
                subagents: Default::default(),
                guardian_review_model: None,
                kind,
                home: std::path::PathBuf::from("/profiles").join(id),
                environment: Default::default(),
            },
        );
    }
    config.targets.insert(
        "podman".into(),
        mj_core::config::TargetTemplate::LocalPodman {
            container: mj_core::config::ContainerTemplate {
                build_cache: None,
                image: "ubuntu:24.04".into(),
                pull_policy: Default::default(),
                platform: None,
                cpus: None,
                memory: None,
                environment: Default::default(),
                workspace_storage: Default::default(),
            },
        },
    );
    config.bundles.insert(
        "hel".into(),
        mj_core::config::ProjectBundle {
            primary_repo: "project".into(),
            repositories: vec![mj_core::config::ProjectRepository {
                id: "project".into(),
                github: Some("owner/project".into()),
                local: None,
                destination: "project".into(),
                git_ref: None,
            }],
        },
    );
    let mut state = State::default();
    for (id, title) in [("session-1", "First"), ("session-2", "Second")] {
        state.sessions.insert(
            id.into(),
            mj_core::state::SessionRecord {
                project: None,
                target_runtime: None,
                launch_base: None,
                launch_branch: None,
                checkout: None,
                publication: None,
                build_cache: None,
                container_workspace: None,
                subagents: None,
                create_managed_worktree: None,
                workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned(),
                archived: false,
                container_cpus: None,
                container_memory: None,
                id: id.into(),
                title: title.into(),
                harness_kind: mj_core::config::HarnessKind::Codex,
                last_profile: "codex-1".into(),
                bundle_id: "hel".into(),
                project_directory: None,
                managed_worktree: None,
                review: None,
                target_template_id: "podman".into(),
                resource_allocation: None,
                additional_mounts: Vec::new(),
                state: mj_core::state::SessionState::Running,
                target: None,
                native_session_id: None,
                acp_session_title: None,
                session_title_override: None,
                created_at: "2026-08-14T00:00:00Z".into(),
                updated_at: "2026-08-14T00:00:00Z".into(),
                viewed_through_event_ordinal: 0,
                draft_input: String::new(),
                last_error: None,
                last_checkpoint_error: None,
                checkpoint: None,
            },
        );
    }
    DashboardState::new(config, state, std::collections::BTreeMap::new())
}

fn escape() -> Event {
    Event::Key(crossterm::event::KeyEvent::new(
        KeyCode::Esc,
        KeyModifiers::NONE,
    ))
}

fn plain_key(code: crossterm::event::KeyCode) -> crossterm::event::KeyEvent {
    crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
}

fn prefix_key() -> crossterm::event::KeyEvent {
    crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('b'),
        crossterm::event::KeyModifiers::CONTROL,
    )
}

fn route(dashboard: &mut DashboardState, keys: &[crossterm::event::KeyEvent]) -> KeyRoute {
    let mut route = KeyRoute::Forward;
    for key in keys {
        route = match dashboard.route_bound_key(key) {
            KeyRoute::Command { id, .. } if !dashboard.command_allowed_now(id) => {
                KeyRoute::Consumed
            }
            decided => decided,
        };
    }
    route
}

#[test]
fn escape_cancels_opening_without_stealing_modal_or_quit_keys() {
    assert!(opening_cancel_event(&escape(), true, false));
    assert!(!opening_cancel_event(&escape(), true, true));
    assert!(!opening_cancel_event(&escape(), false, false));
    let quit = Event::Key(plain_key(KeyCode::Char('q')));
    assert!(!opening_cancel_event(&quit, true, false));
    let mut dashboard = populated_dashboard();
    assert_eq!(
        route(
            &mut dashboard,
            &[prefix_key(), plain_key(KeyCode::Char('q'))]
        ),
        KeyRoute::Command {
            id: CommandId::QuitDetach,
            index: None
        }
    );
}

#[tokio::test]
async fn delayed_attachment_cannot_land_after_a_to_b_to_a_or_layout_restore() {
    let mut dashboard = populated_dashboard();
    dashboard.select_active_session("session-1");
    let pane = dashboard.focused_pane();
    let assignment = dashboard.pane_assignment(pane).unwrap();
    let mut attachment = attachment::SessionAttachment::default();
    attachment.bind(Some(assignment));
    let (finish, wait) = tokio::sync::oneshot::channel();
    let (report, received) = tokio::sync::oneshot::channel();
    attachment.spawn(
        "session-1",
        Duration::from_secs(5),
        async move { wait.await.map_err(|error| error.to_string()) },
        move |generation, result| {
            report.send((generation, result)).unwrap();
        },
    );
    // No supervisor pass occurs between these input events and the reply.
    dashboard.select_active_session("session-2");
    dashboard.select_active_session("session-1");
    finish.send(()).unwrap();
    let (generation, result) = received.await.unwrap();
    assert!(result.is_ok());
    assert!(!attachment.accepts_pane_result(generation, assignment, &dashboard, pane, "session-1"));
    assert_eq!(dashboard.selected_session_id(), Some("session-1"));

    // A new attempt for the current assignment can complete, even if another
    // pane became active while it was loading.
    let current = dashboard.pane_assignment(pane).unwrap();
    attachment.bind(Some(current));
    let (report, received) = tokio::sync::oneshot::channel();
    attachment.spawn(
        "session-1",
        Duration::from_secs(5),
        async { Ok(()) },
        move |generation, _| {
            report.send(generation).unwrap();
        },
    );
    let generation = received.await.unwrap();
    let other = dashboard
        .split_focused_pane(ratatui::layout::Direction::Horizontal, Some("session-2"))
        .unwrap();
    assert!(attachment.accepts_pane_result(generation, current, &dashboard, pane, "session-1"));
    assert_eq!(dashboard.selected_session_id(), Some("session-2"));
    assert_eq!(dashboard.focused_pane(), other);

    let saved = dashboard.export_conversation_layout();
    dashboard.set_active_workspace(Some("another-workspace".into()));
    dashboard.set_active_workspace(Some("default".into()));
    assert_eq!(dashboard.export_conversation_layout(), saved);
    assert!(!attachment.accepts_pane_result(generation, current, &dashboard, pane, "session-1"));
    let restored = dashboard.pane_assignment(pane).unwrap();
    dashboard.close_pane(pane);
    assert!(!attachment.accepts_pane_result(generation, restored, &dashboard, pane, "session-1"));
}

#[tokio::test]
async fn selecting_a_loading_session_never_renders_the_previous_chat() {
    let mut dashboard = populated_dashboard();
    dashboard.select_active_session("session-1");
    let mut chats = BTreeMap::from([("session-1".to_owned(), open_test_chat("session-1"))]);
    dashboard.select_active_session("session-2");
    let pane = dashboard.focused_pane();
    dashboard.set_opening_session(Some("session-2"));
    let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
    let mut drawn = Vec::new();
    terminal
        .draw(|frame| {
            drawn = render_combined(
                frame,
                &mut dashboard,
                &mut chats,
                &BTreeMap::from([(pane, "session-2".to_owned())]),
                false,
            );
        })
        .unwrap();
    assert!(!drawn.iter().any(|id| id == "session-1"));
    assert_eq!(dashboard.selected_session_id(), Some("session-2"));
    assert_eq!(dashboard.current_session_id(), Some("session-2"));
}

#[test]
fn switching_sessions_preserves_drafts_through_the_rendered_attach_composer() {
    let mut dashboard = populated_dashboard();
    let mut cache = ComposerDraftCache::default();
    cache.capture("session-1", "first unsent prompt".into(), "");
    cache.capture("session-2", "second unsent prompt".into(), "");
    let first = live_session("session-1", "2026-09-19T00:00:00Z");
    let second = live_session("session-2", "2026-09-19T00:00:00Z");
    let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();

    for (session, expected) in [
        (&first, "first unsent prompt"),
        (&second, "second unsent prompt"),
        (&first, "first unsent prompt!"),
    ] {
        assert_eq!(
            drafts::prepare_attach_draft(&mut cache, &mut dashboard, session),
            expected
        );
        dashboard.select_active_session(&session.id);
        dashboard.set_current_session(Some(&session.id));
        dashboard.set_opening_session(Some(&session.id));
        dashboard.focus_prompt();
        let pane = dashboard.focused_pane();
        terminal
            .draw(|frame| {
                render_combined(
                    frame,
                    &mut dashboard,
                    &mut BTreeMap::new(),
                    &BTreeMap::from([(pane, session.id.clone())]),
                    false,
                );
            })
            .unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            screen.contains(expected),
            "the pending composer must show the saved draft"
        );
        dashboard.handle_key(plain_key(KeyCode::Char('!')));
    }
    assert_eq!(
        dashboard.take_standby_prompt_draft("session-1").as_deref(),
        Some("first unsent prompt!!")
    );
    assert_eq!(
        dashboard.take_standby_prompt_draft("session-2").as_deref(),
        Some("second unsent prompt!")
    );
}

#[test]
fn clearing_a_draft_during_attach_does_not_restore_the_old_cached_text() {
    let mut dashboard = populated_dashboard();
    let mut cache = ComposerDraftCache::default();
    let mut session = live_session("session-1", "2026-09-19T00:00:00Z");
    session.draft_input = "legacy draft".into();
    drafts::prepare_attach_draft(&mut cache, &mut dashboard, &session);
    dashboard.select_active_session(&session.id);
    dashboard.set_current_session(Some(&session.id));
    dashboard.set_opening_session(Some(&session.id));
    dashboard.focus_prompt();
    dashboard.handle_key(crossterm::event::KeyEvent::new(
        KeyCode::Char('a'),
        KeyModifiers::CONTROL,
    ));
    dashboard.handle_key(crossterm::event::KeyEvent::new(
        KeyCode::Char('k'),
        KeyModifiers::CONTROL,
    ));
    assert_eq!(
        drafts::prepare_attach_draft(&mut cache, &mut dashboard, &session),
        ""
    );
    assert_eq!(cache.open(&session.id, "stale daemon draft").text, "");
}

/// The transcript on screen must belong to the row the selection is on.
/// An attach for another session hides the chat that is still loaded; a
/// reattach of the same one does not, because there is nothing to correct.
// Hard-won: 684cdfdd7c: A different-session attach previously left the old transcript and composer visible under the new row; the attach-visibility guard now hides that stale chat.
#[test]
fn only_an_attach_for_another_session_hides_the_warm_chat() {
    assert!(chat_is_visible(None, "session-a"));
    assert!(chat_is_visible(Some("session-a"), "session-a"));
    assert!(!chat_is_visible(Some("session-b"), "session-a"));
}

/// One conversation in the focused pane, with its row selected: what the
/// renderer needs before it draws a conversation at all.
fn focused_chats(dashboard: &mut DashboardState, chat: ActiveChat) -> BTreeMap<String, ActiveChat> {
    let session_id = chat.session_id().to_owned();
    dashboard.set_current_session(Some(&session_id));
    dashboard.select_active_session(&session_id);
    BTreeMap::from([(session_id, chat)])
}

/// Draws the combined surface exactly as the loop does, so the highlight
/// and the extraction see the frame it just produced. No conversation is
/// attached, which stands for a workspace whose sessions are all stopped.
fn draw_with_selection(
    terminal: &mut Terminal<TestBackend>,
    dashboard: &mut DashboardState,
    selection: &SelectionState,
) -> Option<String> {
    let mut text = None;
    terminal
        .draw(|frame| {
            mj_tui::render_combined_with_theme(
                frame,
                dashboard,
                &mut BTreeMap::new(),
                &BTreeMap::new(),
                false,
                mj_core::config::UiTheme::Midnight,
            );
            text = draw_selection(frame, selection, dashboard.frame_surfaces());
        })
        .expect("draw the combined surface");
    text
}

fn reversed_cells(terminal: &Terminal<TestBackend>) -> Vec<(u16, u16)> {
    let buffer = terminal.backend().buffer();
    (buffer.area.y..buffer.area.bottom())
        .flat_map(|y| (buffer.area.x..buffer.area.right()).map(move |x| (x, y)))
        .filter(|&(x, y)| {
            buffer
                .cell(Position::new(x, y))
                .expect("cell")
                .modifier
                .contains(Modifier::REVERSED)
        })
        .collect()
}

#[tokio::test]
async fn a_remote_stop_marks_the_open_chat_retiring_before_its_feed_closes() {
    let mut stopped = open_test_chat("session-open");
    mark_active_chat_retiring_for_remote_lifecycle(
        Some(&mut stopped),
        "session-open",
        SessionOperationKind::Suspending,
    );
    assert!(stopped.session_retiring());

    let mut launched = open_test_chat("session-open");
    mark_active_chat_retiring_for_remote_lifecycle(
        Some(&mut launched),
        "session-open",
        SessionOperationKind::Launching,
    );
    assert!(!launched.session_retiring());
}

/// A drag inside a pane belongs to that pane: the range stops at its last
/// row even though the pointer left it, and the copied text is the pane's
/// own rows without borders or anything drawn around it.
#[test]
fn dragging_inside_a_pane_copies_only_that_panes_rows() {
    let mut dashboard = populated_dashboard();
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");
    let mut selection = SelectionState::new();
    draw_with_selection(&mut terminal, &mut dashboard, &selection);
    let quotas = *dashboard
        .frame_surfaces()
        .surface(SurfaceId::DashboardPane(2))
        .expect("quotas pane registered");

    let press = (quotas.rect.x, quotas.rect.y + 1);
    // Drag out past the pane's bottom-right corner, into the footer.
    let release = (quotas.rect.right() + 10, quotas.rect.bottom() + 5);
    assert_eq!(
        route_selection_event(
            &mut selection,
            dashboard.frame_surfaces(),
            mouse(MouseEventKind::Down(MouseButton::Left), press.0, press.1),
        ),
        SelectionRouting::Consumed
    );
    assert_eq!(
        route_selection_event(
            &mut selection,
            dashboard.frame_surfaces(),
            mouse(
                MouseEventKind::Drag(MouseButton::Left),
                release.0,
                release.1
            ),
        ),
        SelectionRouting::Consumed
    );
    assert_eq!(
        route_selection_event(
            &mut selection,
            dashboard.frame_surfaces(),
            mouse(MouseEventKind::Up(MouseButton::Left), release.0, release.1),
        ),
        SelectionRouting::Copy {
            surface: SurfaceId::DashboardPane(2),
            range: SelectionRange {
                start: mj_chat::selection::ContentPos::new(1, 0),
                end: mj_chat::selection::ContentPos::new(2, quotas.rect.width - 1),
            },
        }
    );

    let text = draw_with_selection(&mut terminal, &mut dashboard, &selection)
        .expect("the selection covers text");
    assert_eq!(
        text.lines().collect::<Vec<_>>(),
        vec![
            "  claude-1  Claude Code  refreshing…",
            "  codex-1   Codex        refreshing…",
        ]
    );
    // Exactly the two selected pane rows are reversed, border columns and
    // the pane above included.
    let expected = (quotas.rect.y + 1..quotas.rect.bottom())
        .flat_map(|y| (quotas.rect.x..quotas.rect.right()).map(move |x| (x, y)))
        .collect::<Vec<_>>();
    assert_eq!(reversed_cells(&terminal), expected);
}

#[tokio::test]
async fn prompt_press_focuses_before_release_and_preserves_drag_selection() {
    let mut dashboard = populated_dashboard();
    dashboard.focus_sessions();
    let fixture = mj_client::session::replacement_session_test_fixture("session-1", 1);
    let notices = Notices::default();
    let chat = ActiveChat::open(
        fixture.stopped,
        "bundle-1",
        None,
        fixture.control,
        SessionHeaderIdentity::default(),
        "select this prompt text".into(),
        notices.clone(),
    );
    notices.clear();
    let mut chats = focused_chats(&mut dashboard, chat);
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("terminal");
    terminal
        .draw(|frame| {
            render_combined(frame, &mut dashboard, &mut chats, &BTreeMap::new(), false);
        })
        .unwrap();
    let prompt = dashboard
        .frame_surfaces()
        .surface(SurfaceId::PromptInput)
        .unwrap()
        .rect;
    let mut selection = SelectionState::new();
    assert_eq!(
        route_prompt_selection(
            &mut selection,
            &mut dashboard,
            mouse(MouseEventKind::Down(MouseButton::Left), prompt.x, prompt.y)
        ),
        SelectionRouting::Consumed,
    );
    assert!(
        dashboard.prompt_has_focus(),
        "focus must change before mouse-up"
    );
    assert!(
        selection.range().is_none(),
        "the press remains a click candidate"
    );
    assert_eq!(
        route_prompt_selection(
            &mut selection,
            &mut dashboard,
            mouse(
                MouseEventKind::Drag(MouseButton::Left),
                prompt.x + 5,
                prompt.y
            )
        ),
        SelectionRouting::Consumed,
    );
    assert!(selection.range().is_some());
    assert!(matches!(
        route_prompt_selection(
            &mut selection,
            &mut dashboard,
            mouse(
                MouseEventKind::Up(MouseButton::Left),
                prompt.x + 5,
                prompt.y
            )
        ),
        SelectionRouting::Copy {
            surface: SurfaceId::PromptInput,
            ..
        }
    ));
}

/// Launch finding R5-6: the dictation chord's reason went to the
/// conversation's own status line, one line cut at the pane's edge, and never
/// reached Recent messages. It now goes to the shared notices, like the other
/// chords that cannot run.
// Hard-won: 3d0f6d2648: Launch finding R5-6 found the dictation failure clipped in chat status and absent from Recent messages; this checks the shared notice path.
#[tokio::test]
async fn the_dictation_chord_reports_why_it_cannot_run_in_the_shared_notices() {
    let mut dashboard = populated_dashboard();
    let notices = Notices::default();
    dashboard.share_notices(notices.clone());
    let mut chat = open_test_chat_with_notices("dictation-unavailable", notices.clone());

    super::actions::apply_chat_toggle(
        &mut dashboard,
        Some(&mut chat),
        super::actions::ChatToggle::Dictation,
    );

    let notice = dashboard.notice().expect("the chord explains itself");
    assert!(notice.starts_with("Dictation is unavailable"), "{notice}");
    assert!(
        notices.history().iter().any(|record| record.text == notice),
        "the reason is kept for Recent messages"
    );
    assert!(
        !chat
            .notice()
            .is_some_and(|own| own.starts_with("Dictation")),
        "not in the conversation's own line"
    );
}

/// Launch finding R5-7: during a session's first turn the Sessions row showed
/// the harness's title while the conversation header still said "project via
/// fake", until the turn ended. The worker poll that gives the record its
/// title now refreshes the open conversation too, so both read the record's
/// listed title.
// Hard-won: f2c36f6900: Harness title updates changed the session row but left the open conversation stale until the turn ended; the test checks the title reaches the header immediately.
#[tokio::test]
async fn a_title_from_the_harness_reaches_the_conversation_header_with_the_row() {
    let session_id = "title-lag";
    let mut record = live_session(session_id, "2026-09-24T22:30:00Z");
    record.title = "project via fake".into();
    let mut controller = mj_controller::controller::Controller {
        config: Config::default(),
        state: State {
            sessions: [(session_id.to_owned(), record.clone())]
                .into_iter()
                .collect(),
            ..State::default()
        },
    };
    let mut dashboard = populated_dashboard();
    dashboard.set_state(controller.state.clone());
    let fixture = mj_client::session::replacement_session_test_fixture(session_id, 1);
    let chat = ActiveChat::open(
        fixture.stopped,
        "bundle-1",
        Some(mj_chat::chat::ChatSessionContext {
            config: controller.config.clone(),
            session: record.clone(),
            reviewer_stager: mj_controller::controller::reviewer_stager(),
        }),
        fixture.control,
        SessionHeaderIdentity {
            title: record.listed_title().to_owned(),
            ..SessionHeaderIdentity::default()
        },
        String::new(),
        Notices::default(),
    );
    let mut chats = BTreeMap::from([(session_id.to_owned(), chat)]);
    assert_eq!(chats[session_id].header_title(), "project via fake");

    let mut materialized = mj_core::state::MaterializedSession::empty(session_id);
    materialized.session_title = Some("r5 suspend probe".into());
    let update = crate::pollers::WorkerPollUpdate {
        session_id: session_id.to_owned(),
        view: mj_controller::session_manager::ManagedSessionView {
            snapshot: Some(mj_core::state::ManagedSessionSnapshot {
                window: mj_core::state::ProjectionWindow::of(&materialized),
                materialized,
                operational: mj_core::relay::RelaySnapshot::new(session_id.to_owned())
                    .operational_state(),
                latest_credential_sync_signal: None,
                worker_build: None,
                subagent_requests: Vec::new(),
                subagent_results: Vec::new(),
            }),
            connected: true,
            error: None,
        },
    };

    let retitled = crate::pollers::apply_worker_poll_update(
        &mut controller,
        &mut dashboard,
        &mut chats,
        update,
        None,
    )
    .unwrap();

    assert!(retitled.is_some());
    assert_eq!(
        controller.state.sessions[session_id].listed_title(),
        "r5 suspend probe",
        "the Sessions row's title"
    );
    assert_eq!(chats[session_id].header_title(), "r5 suspend probe");
}

fn live_session(id: &str, created_at: &str) -> mj_core::state::SessionRecord {
    mj_core::state::SessionRecord {
        project: None,
        target_runtime: None,
        launch_base: None,
        launch_branch: None,
        checkout: None,
        publication: None,
        build_cache: None,
        container_workspace: None,
        subagents: None,
        create_managed_worktree: None,
        workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned(),
        archived: false,
        container_cpus: None,
        container_memory: None,
        id: id.into(),
        title: id.into(),
        harness_kind: mj_core::config::HarnessKind::Codex,
        last_profile: "codex-1".into(),
        bundle_id: "hel".into(),
        project_directory: None,
        managed_worktree: None,
        review: None,
        target_template_id: "podman".into(),
        resource_allocation: None,
        additional_mounts: Vec::new(),
        state: mj_core::state::SessionState::Running,
        target: None,
        native_session_id: None,
        acp_session_title: None,
        session_title_override: None,
        created_at: created_at.into(),
        updated_at: created_at.into(),
        viewed_through_event_ordinal: 0,
        draft_input: String::new(),
        last_error: None,
        last_checkpoint_error: None,
        checkpoint: None,
    }
}

#[test]
fn startup_never_picks_a_session_whose_target_failed() {
    let mut failed = live_session("failed", "2026-08-03T00:00:00Z");
    failed.state = mj_core::state::SessionState::Error;
    let healthy = live_session("healthy", "2026-08-01T00:00:00Z");
    let activity = |id: &str| match id {
        "failed" => Some(300),
        "healthy" => Some(100),
        _ => None,
    };
    // The failed session is newer and busier, and still not the pick.
    assert_eq!(
        startup_session_choice(
            Some(mj_core::workspace::DEFAULT_WORKSPACE_ID),
            [&failed, &healthy],
            activity
        ),
        Some("healthy".into())
    );
    assert_eq!(
        startup_session_choice(
            Some(mj_core::workspace::DEFAULT_WORKSPACE_ID),
            [&failed],
            activity
        ),
        None
    );
}

#[test]
fn startup_activity_in_another_workspace_cannot_replace_the_opened_workspace() {
    let local = live_session("local", "2026-08-01T00:00:00Z");
    let mut foreign = live_session("foreign", "2026-08-03T00:00:00Z");
    foreign.workspace_id = "another-workspace".into();
    let mut state = mj_core::state::State::default();
    state.sessions.insert(local.id.clone(), local.clone());
    state.sessions.insert(foreign.id.clone(), foreign);
    let mut dashboard = DashboardState::new(Default::default(), state, Default::default());
    dashboard.set_active_workspace(Some(local.workspace_id.clone()));
    assert_eq!(
        startup_session_choice(
            dashboard.active_workspace_id(),
            dashboard.startup_sessions(),
            |id| { Some(if id == "foreign" { 10_000 } else { 1 }) }
        ),
        Some(local.id)
    );
}

/// With no activity recorded — nothing stored yet, or every read failed —
/// every session ranks equal on the first key, so the newest one wins.
#[test]
fn startup_falls_back_to_the_newest_creation_then_the_larger_id() {
    let sessions = [
        live_session("session-a", "2026-08-01T00:00:00Z"),
        live_session("session-b", "2026-08-03T00:00:00Z"),
        live_session("session-c", "2026-08-02T00:00:00Z"),
    ];
    assert_eq!(
        startup_session_choice(
            Some(mj_core::workspace::DEFAULT_WORKSPACE_ID),
            sessions.iter(),
            |_| None
        ),
        Some("session-b".into())
    );

    // A tie on creation time too still resolves the same way on every
    // run, rather than following the iteration order.
    let tied = [
        live_session("session-a", "2026-08-01T00:00:00Z"),
        live_session("session-z", "2026-08-01T00:00:00Z"),
    ];
    assert_eq!(
        startup_session_choice(
            Some(mj_core::workspace::DEFAULT_WORKSPACE_ID),
            tied.iter(),
            |_| None
        ),
        Some("session-z".into())
    );
    assert_eq!(
        startup_session_choice(
            Some(mj_core::workspace::DEFAULT_WORKSPACE_ID),
            tied.iter().rev(),
            |_| None
        ),
        Some("session-z".into())
    );
}

/// The pick waits for the summaries it compares, but not for ever, and it
/// only ever fires once.
#[test]
fn the_startup_pick_waits_for_its_summaries_then_gives_up() {
    let start = std::time::Instant::now();
    let mut startup =
        StartupSession::begin(["session-a".to_owned(), "session-b".to_owned()], start);

    assert!(!startup.ready(start), "both summaries are still pending");
    startup.summary_arrived("session-a");
    assert!(!startup.ready(start), "one summary is still pending");
    startup.summary_arrived("session-b");
    assert!(startup.ready(start));
    assert!(!startup.ready(start), "the choice is made only once");

    // A summary that never comes back stops holding the surface up.
    let mut stalled = StartupSession::begin(["session-a".to_owned()], start);
    assert!(!stalled.ready(start));
    assert!(stalled.ready(start + STARTUP_SESSION_WAIT));
}

/// Automatic attachments wait for startup to restore the saved arrangement
/// or choose a fresh conversation. User input cancels that choice and lets
/// attachment reconciliation follow the user's assignments immediately.
#[test]
fn the_pick_holds_the_automatic_follow_back_until_it_has_run() {
    let start = std::time::Instant::now();
    let mut startup = StartupSession::begin(["session-a".to_owned()], start);

    assert!(startup.pick_pending());
    startup.summary_arrived("session-a");
    assert!(startup.pick_pending(), "the pick has still to run");
    assert!(startup.ready(start));
    assert!(!startup.pick_pending(), "the pick has run");

    // A user who acts first takes the choice, and the follow resumes at once.
    let mut acted = StartupSession::begin(["session-a".to_owned()], start);
    acted.cancel();
    assert!(!acted.pick_pending());

    // An empty workspace has nothing to wait for.
    assert!(!StartupSession::begin(std::iter::empty(), start).pick_pending());
}

/// The user acting is the strongest signal there is about which
/// conversation they want, so it takes the choice away.
#[test]
fn a_user_who_acts_first_keeps_the_choice() {
    let start = std::time::Instant::now();
    let mut startup = StartupSession::begin(["session-a".to_owned()], start);

    startup.cancel();
    startup.summary_arrived("session-a");
    assert!(!startup.ready(start));
    assert!(!startup.ready(start + STARTUP_SESSION_WAIT * 10));
}

#[test]
fn materialized_projections_are_single_flight_and_coalesce_to_the_latest_snapshot() {
    let mut in_flight = BTreeSet::new();
    let mut pending = BTreeMap::new();
    let mut first = MaterializedSession::empty("session-1");
    first.applied_event_ordinal = 1;
    let mut superseded = MaterializedSession::empty("session-1");
    superseded.applied_event_ordinal = 2;
    let mut latest = MaterializedSession::empty("session-1");
    latest.applied_event_ordinal = 3;
    let mut stale = MaterializedSession::empty("session-1");
    stale.applied_event_ordinal = 2;

    assert!(enqueue_materialized_projection(&mut in_flight, &mut pending, first, 0).is_some());
    assert!(enqueue_materialized_projection(&mut in_flight, &mut pending, superseded, 1).is_none());
    assert!(enqueue_materialized_projection(&mut in_flight, &mut pending, latest, 2).is_none());
    assert!(enqueue_materialized_projection(&mut in_flight, &mut pending, stale, 1).is_none());

    let (queued, receipt) = pending.remove("session-1").unwrap();
    assert_eq!(queued.applied_event_ordinal, 3);
    assert_eq!(receipt, 2);
    assert_eq!(in_flight, BTreeSet::from(["session-1".to_owned()]));
}

#[test]
fn critical_operations_hold_shutdown_until_their_guards_drop() {
    let (tracker, changed) = CriticalOperationTracker::new();
    let first = tracker.begin("saving draft for 01234567");
    let second = tracker.begin("stopping session 89abcdef");

    assert_eq!(tracker.blockers().len(), 2);
    assert_eq!(
        shutdown_wait_notice(&tracker.blockers()).as_deref(),
        Some("Waiting for 2 operations to complete before exiting")
    );
    assert!(changed.has_changed().unwrap());

    drop(first);
    assert_eq!(
        shutdown_wait_notice(&tracker.blockers()).as_deref(),
        Some("Waiting for stopping session 89abcdef to complete before exiting")
    );
    drop(second);
    assert_eq!(shutdown_wait_notice(&tracker.blockers()), None);
}

/// Every warm conversation is pumped on every loop iteration, not only the
/// one the focused pane shows. Each of these chats starts on a stopped
/// session handle and only reaches its replacement actor while it is being
/// pumped, so acquiring both replacement actors proves both were driven.
#[tokio::test]
async fn every_warm_chat_is_pumped_not_only_the_focused_one() {
    let mut fixtures = Vec::new();
    let mut notices = Vec::new();
    let mut chats = BTreeMap::new();
    for session_id in ["session-1", "session-2"] {
        let fixture = mj_client::session::replacement_session_test_fixture(session_id, 1);
        let chat_notices = Notices::default();
        chats.insert(
            session_id.to_owned(),
            ActiveChat::open(
                fixture.stopped.clone(),
                "bundle-1",
                None,
                fixture.control.clone(),
                SessionHeaderIdentity::default(),
                String::new(),
                chat_notices.clone(),
            ),
        );
        notices.push(chat_notices);
        fixtures.push(fixture);
    }

    tokio::time::timeout(Duration::from_secs(5), async {
        while !chats
            .values()
            .all(|chat| chat.session_feed_open() && chat.notice().is_none())
        {
            super::pump_chats(&mut chats).await;
        }
    })
    .await
    .expect("both warm conversations reconnected while being pumped");

    assert!(
        chats
            .values()
            .all(mj_chat::chat::ActiveChat::session_feed_open)
    );
    assert!(notices.iter().all(|notices| notices.current().is_none()));
}

/// Launch findings B-2 and D-1: the pane chrome ("Browse | Conversation")
/// was drawn over the start of the conversation's own title, so the title
/// row read `Conversation e` for Idle. The title must start after the
/// chrome, so the target and the state word stay whole.
// Hard-won: bf7797a1ca: Launch findings B-2/D-1 recorded pane chrome overwriting the conversation title; the render assertion checks the target, profile, and title stay visible.
#[tokio::test]
async fn the_pane_chrome_does_not_cover_the_conversation_title() {
    let mut dashboard = populated_dashboard();
    let fixture = mj_client::session::replacement_session_test_fixture("session-1", 1);
    let chat = ActiveChat::open(
        fixture.stopped,
        "bundle-1",
        None,
        fixture.control,
        SessionHeaderIdentity {
            target: "podman-target".into(),
            profile: "fake".into(),
            title: "S4".into(),
            ..SessionHeaderIdentity::default()
        },
        String::new(),
        Notices::default(),
    );
    let mut chats = focused_chats(&mut dashboard, chat);
    let mut terminal = Terminal::new(TestBackend::new(140, 40)).expect("terminal");
    terminal
        .draw(|frame| {
            render_combined(frame, &mut dashboard, &mut chats, &BTreeMap::new(), false);
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    let rows = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>();
    let title = rows
        .iter()
        .find(|row| row.contains("| Conversation"))
        .unwrap_or_else(|| panic!("no pane title row:\n{}", rows.join("\n")));
    assert!(title.contains("| Conversation  podman-target  "), "{title}");
    assert!(title.contains("  fake  S4"), "{title}");
}

/// Launch finding D-11: in a narrow pane the title row read
/// `Browse |t ◇r`: the chrome label was cut without an ellipsis, a letter of
/// the conversation title showed between the label and the pin chip, and
/// another leaked through the chip's unpainted third cell. At every width
/// the row must show whole words or an ellipsis, never stray letters.
// Hard-won: 1881eaf49c: Launch finding D-11 found title letters leaking past truncated chrome and pin chips; the width sweep rejects stray letters across 40–140 columns.
#[tokio::test]
async fn a_narrow_pane_title_ends_in_an_ellipsis_not_stray_letters() {
    for width in (40..=140).step_by(3) {
        let mut dashboard = populated_dashboard();
        let fixture = mj_client::session::replacement_session_test_fixture("session-1", 1);
        let chat = ActiveChat::open(
            fixture.stopped,
            "bundle-1",
            None,
            fixture.control,
            SessionHeaderIdentity {
                target: "podman-target".into(),
                profile: "fake".into(),
                title: "S4".into(),
                ..SessionHeaderIdentity::default()
            },
            String::new(),
            Notices::default(),
        );
        let mut chats = focused_chats(&mut dashboard, chat);
        let mut terminal = Terminal::new(TestBackend::new(width, 30)).expect("terminal");
        terminal
            .draw(|frame| {
                render_combined(frame, &mut dashboard, &mut chats, &BTreeMap::new(), false);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let Some(row) = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect::<Vec<_>>()
            })
            .find(|cells| cells.concat().contains(" Browse "))
        else {
            continue;
        };
        let text = row.concat();
        for pair in row.windows(2) {
            let (left, right) = (pair[0].as_str(), pair[1].as_str());
            let letter = right.chars().next().is_some_and(char::is_alphanumeric) || right == "…";
            assert!(
                !(["|", "◇", "◆", "⋯", "×"].contains(&left) && letter),
                "width {width}: {text}"
            );
        }
    }
}

// Hard-won: f17e582733: A reconnect could leave the unavailable notice in place; the test checks the notice is replaced when the daemon returns.
#[test]
fn a_quick_reconnect_replaces_the_daemon_unavailable_notice() {
    let mut dashboard =
        DashboardState::new(Default::default(), State::default(), Default::default());
    dashboard.set_failure_notice(DAEMON_UNAVAILABLE_NOTICE);
    show_daemon_reattached(&mut dashboard);
    assert_eq!(
        dashboard.notice().as_deref(),
        Some(DAEMON_RUNNING_AGAIN_NOTICE)
    );

    dashboard.set_failure_notice("Could not save the layout.");
    dashboard.set_failure_notice(DAEMON_UNAVAILABLE_NOTICE);
    show_daemon_reattached(&mut dashboard);
    assert_eq!(
        dashboard.notice().as_deref(),
        Some(DAEMON_RUNNING_AGAIN_NOTICE)
    );

    // Another fresh failure is not the reconnect's to clear.
    dashboard.set_failure_notice("Could not save the layout.");
    show_daemon_reattached(&mut dashboard);
    assert_eq!(
        dashboard.notice().as_deref(),
        Some("Could not save the layout.")
    );
}

// Hard-won: ad49507a60: A reconnect previously replaced only the unavailable notice, leaving the stopped-daemon error; the test checks that error is cleared.
#[test]
fn a_reconnect_replaces_the_daemon_not_running_error_notice() {
    // A failed request against a stopped daemon reports the client's own
    // "not running" error as a failure notice. Nothing but the reconnect
    // clears it while the dashboard idles, so the reconnect must.
    let mut dashboard =
        DashboardState::new(Default::default(), State::default(), Default::default());
    let stopped = mj_client::daemon::DaemonNotRunning {
        metadata_path: "daemon.json".into(),
    }
    .to_string();
    dashboard.set_failure_notice(stopped);
    show_daemon_reattached(&mut dashboard);
    assert_eq!(
        dashboard.notice().as_deref(),
        Some(DAEMON_RUNNING_AGAIN_NOTICE)
    );
}

// Hard-won: ad49507a60: A missed Missing-to-Attached event left a daemon failure until keypress; an attached keep-alive now clears it on a tick.
#[test]
fn an_attached_keep_alive_clears_a_daemon_failure_without_a_transition() {
    // The keep-alive recovered without the dashboard handling the
    // Missing-to-Attached change (or the failure landed after it). The
    // connection state alone must still take the failure down, and arm the
    // reconnect notice's expiry.
    use crate::daemon::DaemonPresence;
    let mut dashboard =
        DashboardState::new(Default::default(), State::default(), Default::default());
    let now = std::time::Instant::now();
    let mut since = None;

    dashboard.set_failure_notice(DAEMON_UNAVAILABLE_NOTICE);
    let missing = DaemonPresence::Missing("gone".into());
    assert!(!reconcile_daemon_failure(
        &mut dashboard,
        &missing,
        &mut since,
        now
    ));
    assert_eq!(
        dashboard.notice().as_deref(),
        Some(DAEMON_UNAVAILABLE_NOTICE)
    );

    assert!(reconcile_daemon_failure(
        &mut dashboard,
        &DaemonPresence::Attached,
        &mut since,
        now
    ));
    assert_eq!(
        dashboard.notice().as_deref(),
        Some(DAEMON_RUNNING_AGAIN_NOTICE)
    );
    assert_eq!(since, Some(now));

    // An unrelated failure is not the connection state's to clear.
    dashboard.set_failure_notice("Could not save the layout.");
    let mut since = None;
    assert!(!reconcile_daemon_failure(
        &mut dashboard,
        &DaemonPresence::Attached,
        &mut since,
        now
    ));
    assert_eq!(
        dashboard.notice().as_deref(),
        Some("Could not save the layout.")
    );
}

// Hard-won: 98c04fa29d: The reconnect notice had remained for over 16 minutes on an idle dashboard; the clock tick now expires it and preserves replacement notices.
#[test]
fn the_daemon_running_again_notice_expires_by_itself() {
    let notices = mj_chat::chat::Notices::default();
    notices.set(DAEMON_RUNNING_AGAIN_NOTICE);
    let shown_at = std::time::Instant::now();
    let mut since = Some(shown_at);

    // Still readable: it stays.
    assert!(!expire_daemon_running_again(
        &notices,
        &mut since,
        shown_at + DAEMON_RUNNING_AGAIN_DISPLAY / 2
    ));
    assert_eq!(
        notices.current().as_deref(),
        Some(DAEMON_RUNNING_AGAIN_NOTICE)
    );

    // Past its display time with no key press: the bar clears.
    assert!(expire_daemon_running_again(
        &notices,
        &mut since,
        shown_at + DAEMON_RUNNING_AGAIN_DISPLAY
    ));
    assert_eq!(notices.current(), None);
    assert_eq!(since, None);

    // A different notice that has replaced it is not this expiry's to clear.
    notices.set(DAEMON_RUNNING_AGAIN_NOTICE);
    let mut since = Some(shown_at);
    notices.set("Could not save the layout.");
    assert!(!expire_daemon_running_again(
        &notices,
        &mut since,
        shown_at + DAEMON_RUNNING_AGAIN_DISPLAY * 2
    ));
    assert_eq!(
        notices.current().as_deref(),
        Some("Could not save the layout.")
    );
}

#[tokio::test]
async fn shutdown_persistence_bounds_a_silent_acknowledgement() {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(20);
    let (saved, silent) = tokio::join!(
        finish_shutdown_save("layout", deadline, async { Ok(()) }),
        finish_shutdown_save("last workspace", deadline, std::future::pending()),
    );
    saved.unwrap();
    let message = silent.unwrap_err().to_string();
    assert!(message.contains("last workspace"));
    assert!(message.contains("may still complete"));
}

#[tokio::test]
async fn delayed_lifecycle_completion_cannot_retire_replacement_chat_or_attach() {
    let mut chats = BTreeMap::from([("session".into(), open_test_chat("session"))]);
    let mut attachments = BTreeMap::new();
    let captured = attachment::ChatRetirement::capture("session", &chats, &attachments);
    assert!(captured.is_current(&chats, &attachments));
    chats.insert("session".into(), open_test_chat("session"));
    assert!(!captured.is_current(&chats, &attachments));

    let captured = attachment::ChatRetirement::capture("session", &chats, &attachments);
    let pane = populated_dashboard().browse_pane();
    let attachment = attachments.entry(pane).or_default();
    attachment.spawn(
        "session",
        Duration::from_secs(30),
        std::future::pending::<Result<(), String>>(),
        |_, _| {},
    );
    assert!(!captured.is_current(&chats, &attachments));
    captured.retire_attachments(&mut attachments);
    // Retiring an old operation leaves the new attachment selected and alive.
    assert!(!attachments.get_mut(&pane).unwrap().select("session"));
}

/// The runtime feed carries only live sessions, so a chat can still be
/// detaching after its suspended session's record has left. Its last record
/// says where the draft and read position belong; nothing reports the
/// session as unknown.
#[tokio::test]
async fn a_chat_detaching_after_its_session_left_the_feed_still_saves_its_draft() {
    let mut controller = Controller {
        config: Config::default(),
        state: State::default(),
    };
    let mut dashboard = DashboardState::new(Config::default(), State::default(), BTreeMap::new());
    let departed = live_session("session-1", "2026-09-19T00:00:00Z");
    let (updates, _received) = tokio::sync::mpsc::unbounded_channel();
    let (tracker, _changed) = CriticalOperationTracker::new();
    let detached = || DetachedChatState {
        client_id: "client",
        session_id: "session-1",
        event_ordinal: 4,
        draft: DetachedSessionDraft {
            text: "unsent".into(),
            inherited_input: None,
        },
    };

    let saved = record_chat_detach_state(
        &mut controller,
        Some(&departed),
        &mut dashboard,
        detached(),
        &updates,
        tracker.clone(),
    )
    .expect("the draft is saved");
    // The current-thread runtime has not polled the save; it never reaches a
    // daemon.
    saved.abort();
    assert_eq!(dashboard.notice(), None);

    assert!(
        record_chat_detach_state(
            &mut controller,
            None,
            &mut dashboard,
            detached(),
            &updates,
            tracker,
        )
        .is_none()
    );
    assert!(
        dashboard
            .notice()
            .is_some_and(|notice| notice.contains("unknown session"))
    );
}
