#[test]
fn detaching_hidden_chat_preserves_unread_background_events() {
    assert_eq!(super::detach_read_frontier(false, 20, 12), 12);
    assert_eq!(super::detach_read_frontier(true, 20, 12), 20);
    assert_eq!(super::detach_read_frontier(true, 10, 12), 12);
}

use super::*;
use mj_chat::chat::{ActiveChat, Notices, SessionHeaderIdentity};
use mj_chat::selection::SurfaceFrame;
use mj_core::state::State;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::{Position, Rect};
use ratatui::style::Modifier;

fn mouse(kind: MouseEventKind, column: u16, row: u16) -> Event {
    Event::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    })
}

fn escape() -> Event {
    Event::Key(crossterm::event::KeyEvent::new(
        KeyCode::Esc,
        KeyModifiers::NONE,
    ))
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
                guardian_review_model: None,
                kind,
                home: std::path::PathBuf::from("/profiles").join(id),
                environment: std::collections::BTreeMap::new(),
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
                environment: std::collections::BTreeMap::new(),
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
                build_cache: None,
                container_workspace: None,
                mjolnir_subagents: None,
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

/// The loop draws once per wakeup. The once-a-second clock is one of the
/// two wakeups allowed to decline that frame, and it declines when none of
/// the values it polls has moved.
#[test]
fn an_unchanged_clock_tick_does_not_redraw() {
    let mut dashboard = populated_dashboard();
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");
    terminal
        .draw(|frame| {
            render_combined(
                frame,
                &mut dashboard,
                &mut BTreeMap::new(),
                &BTreeMap::new(),
                false,
            );
        })
        .expect("draw the combined surface");
    dashboard.acknowledge_render();
    let drawn = terminal.backend().buffer().clone();

    // This is the whole condition the clock arm evaluates for the
    // dashboard. Nothing on this surface advances once a second.
    assert!(!dashboard.clock_changed());

    // And the frame it declines would have been the same one.
    terminal
        .draw(|frame| {
            render_combined(
                frame,
                &mut dashboard,
                &mut BTreeMap::new(),
                &BTreeMap::new(),
                false,
            );
        })
        .expect("draw the combined surface");
    assert_eq!(terminal.backend().buffer(), &drawn);
}

/// Nothing asks for a frame any more. A background feed applies its update
/// through `drain_feeds`, and the frame the loop draws for that wakeup
/// carries it to the screen with no mutation having marked anything.
#[test]
fn a_feed_update_redraws_without_a_dirty_mark() {
    let mut dashboard = populated_dashboard();
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");
    terminal
        .draw(|frame| {
            render_combined(
                frame,
                &mut dashboard,
                &mut BTreeMap::new(),
                &BTreeMap::new(),
                false,
            );
        })
        .expect("draw the combined surface");
    dashboard.acknowledge_render();
    let before = terminal.backend().buffer().clone();

    dashboard.apply_quota(mj_client::quota::ProfileQuota {
        profile_id: "codex-1".into(),
        harness: mj_core::config::HarnessKind::Codex,
        windows: vec![mj_client::quota::QuotaWindow {
            label: "weekly".into(),
            remaining_percent: Some(42),
            used: None,
            limit: None,
            resets: None,
            resets_at_epoch_seconds: None,
        }],
        extra: None,
        error: None,
        refreshed_at_epoch_seconds: mj_core::clock::epoch_seconds(),
    });
    terminal
        .draw(|frame| {
            render_combined(
                frame,
                &mut dashboard,
                &mut BTreeMap::new(),
                &BTreeMap::new(),
                false,
            );
        })
        .expect("draw the combined surface");
    assert_ne!(
        terminal.backend().buffer(),
        &before,
        "the quota reached the screen without anything marking it dirty"
    );
}

/// The transcript on screen must belong to the row the selection is on.
/// An attach for another session hides the chat that is still loaded; a
/// reattach of the same one does not, because there is nothing to correct.
#[test]
fn only_an_attach_for_another_session_hides_the_warm_chat() {
    assert!(chat_is_visible(None, "session-a"));
    assert!(chat_is_visible(Some("session-a"), "session-a"));
    assert!(!chat_is_visible(Some("session-b"), "session-a"));
}

#[tokio::test]
async fn a_remote_stop_marks_the_open_chat_retiring_before_its_feed_closes() {
    let mut stopped = open_test_chat("session-open");
    mark_active_chat_retiring_for_remote_lifecycle(
        Some(&mut stopped),
        "session-open",
        SessionOperationKind::Stopping,
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
            render_combined(
                frame,
                dashboard,
                &mut BTreeMap::new(),
                &BTreeMap::new(),
                false,
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

/// Screen row of the first line holding `needle`.
fn row_containing(terminal: &Terminal<TestBackend>, needle: &str) -> u16 {
    let buffer = terminal.backend().buffer();
    (buffer.area.y..buffer.area.bottom())
        .find(|y| {
            (buffer.area.x..buffer.area.right())
                .map(|x| buffer[(x, *y)].symbol())
                .collect::<String>()
                .contains(needle)
        })
        .unwrap_or_else(|| panic!("missing {needle} on screen"))
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

#[test]
fn short_bordered_minimized_list_can_be_selected_and_copied() {
    let mut dashboard = populated_dashboard();
    dashboard.set_pane_size(mj_tui::SupportPane::Sessions, mj_tui::PaneSize::Minimized);
    let mut terminal = Terminal::new(TestBackend::new(120, 20)).expect("terminal");
    let mut selection = SelectionState::new();
    draw_with_selection(&mut terminal, &mut dashboard, &selection);
    let surface = *dashboard
        .frame_surfaces()
        .surface(SurfaceId::DashboardPane(0))
        .expect("tiny minimized sessions list registered");
    assert_eq!(surface.rect.height, 14);

    let start = (surface.rect.x, surface.rect.y);
    let end = (surface.rect.right() - 1, surface.rect.bottom() - 1);
    assert_eq!(
        route_selection_event(
            &mut selection,
            dashboard.frame_surfaces(),
            mouse(MouseEventKind::Down(MouseButton::Left), start.0, start.1),
        ),
        SelectionRouting::Consumed
    );
    assert_eq!(
        route_selection_event(
            &mut selection,
            dashboard.frame_surfaces(),
            mouse(MouseEventKind::Drag(MouseButton::Left), end.0, end.1),
        ),
        SelectionRouting::Consumed
    );
    assert!(matches!(
        route_selection_event(
            &mut selection,
            dashboard.frame_surfaces(),
            mouse(MouseEventKind::Up(MouseButton::Left), end.0, end.1),
        ),
        SelectionRouting::Copy {
            surface: SurfaceId::DashboardPane(0),
            ..
        }
    ));

    let copied = draw_with_selection(&mut terminal, &mut dashboard, &selection)
        .expect("tiny minimized list selection extracts text");
    assert!(copied.contains("session"), "copied list text: {copied:?}");
}

/// A press is held back until the button comes up, then replayed to the
/// view, so clicking still selects and a second gesture still opens.
#[test]
fn click_gestures_reach_the_view_as_presses_and_still_double_click() {
    let mut dashboard = populated_dashboard();
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");
    let mut selection = SelectionState::new();
    draw_with_selection(&mut terminal, &mut dashboard, &selection);
    let row = row_containing(&terminal, "session-2");
    let column = dashboard
        .frame_surfaces()
        .surface(SurfaceId::DashboardPane(0))
        .expect("active pane registered")
        .rect
        .x;

    let mut click = || {
        assert_eq!(
            route_selection_event(
                &mut selection,
                dashboard.frame_surfaces(),
                mouse(MouseEventKind::Down(MouseButton::Left), column, row),
            ),
            SelectionRouting::Consumed,
            "the press waits for the release"
        );
        let SelectionRouting::Forward(event) = route_selection_event(
            &mut selection,
            dashboard.frame_surfaces(),
            mouse(MouseEventKind::Up(MouseButton::Left), column, row),
        ) else {
            panic!("a release without movement forwards a press");
        };
        assert_eq!(
            event,
            mouse(MouseEventKind::Down(MouseButton::Left), column, row)
        );
        dashboard_event_action(&mut dashboard, event)
    };

    assert_eq!(click(), DashboardAction::None, "the first click selects");
    assert_eq!(
        click(),
        DashboardAction::Open {
            session_id: "session-2".into(),
        },
        "a second gesture on the same row opens it"
    );
}

#[test]
fn presses_off_every_surface_and_wheel_events_reach_the_view() {
    let mut surfaces = FrameSurfaces::new();
    surfaces.push(SurfaceFrame::fixed(
        SurfaceId::ModalBody,
        Rect::new(10, 5, 20, 4),
    ));
    let mut selection = SelectionState::new();

    let outside = mouse(MouseEventKind::Down(MouseButton::Left), 2, 2);
    assert_eq!(
        route_selection_event(&mut selection, &surfaces, outside.clone()),
        SelectionRouting::Forward(outside)
    );
    assert_eq!(selection.active_surface(), None);

    // The wheel scrolls whatever it is over, even inside a surface.
    let wheel = mouse(MouseEventKind::ScrollDown, 12, 6);
    assert_eq!(
        route_selection_event(&mut selection, &surfaces, wheel.clone()),
        SelectionRouting::Forward(wheel)
    );
    // A drag that never started on a surface is the view's too.
    let drag = mouse(MouseEventKind::Drag(MouseButton::Left), 12, 6);
    assert_eq!(
        route_selection_event(&mut selection, &surfaces, drag.clone()),
        SelectionRouting::Forward(drag)
    );
}

#[tokio::test]
async fn transcript_scrollbar_gestures_bypass_text_selection() {
    let mut chat = open_test_chat("scrollbar-selection");
    let mut terminal = Terminal::new(TestBackend::new(60, 20)).expect("terminal");
    terminal
        .draw(|frame| {
            chat.draw_in(
                frame,
                mj_chat::chat::ChatRegions {
                    transcript: Rect::new(0, 0, 60, 15),
                    prompt: Rect::new(0, 15, 60, 5),
                    footer: None,
                    overlay: frame.area(),
                    title_controls: 0,
                    pane_focused: false,
                },
                true,
                false,
            );
        })
        .expect("draw chat");
    let surfaces = chat.frame_surfaces();
    let transcript = surfaces.surface(SurfaceId::Transcript).expect("transcript");
    let scrollbar_x = transcript.rect.right();
    let mut selection = SelectionState::new();
    for event in [
        mouse(MouseEventKind::Down(MouseButton::Left), scrollbar_x, 5),
        mouse(MouseEventKind::Drag(MouseButton::Left), 30, 10),
        mouse(MouseEventKind::Up(MouseButton::Left), 30, 18),
    ] {
        assert_eq!(
            route_selection_event(&mut selection, surfaces, event.clone()),
            SelectionRouting::Forward(event)
        );
    }
    assert!(selection.active_surface().is_none());
    assert_eq!(
        route_selection_event(
            &mut selection,
            surfaces,
            mouse(MouseEventKind::Down(MouseButton::Left), 2, 5),
        ),
        SelectionRouting::Consumed
    );
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

#[test]
fn only_a_visible_question_focuses_the_composer_from_modal_body_clicks() {
    let mut surfaces = FrameSurfaces::new();
    surfaces.push(SurfaceFrame::fixed(
        SurfaceId::ElicitationMessage,
        Rect::new(10, 5, 20, 4),
    ));
    surfaces.push(SurfaceFrame::fixed(
        SurfaceId::ModalBody,
        Rect::new(10, 9, 20, 4),
    ));
    let message_click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 11,
        row: 6,
        modifiers: KeyModifiers::NONE,
    };
    let body_click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 11,
        row: 10,
        modifiers: KeyModifiers::NONE,
    };
    assert!(question_click_focuses(false, &surfaces, &message_click));
    assert!(question_click_focuses(false, &surfaces, &body_click));
    assert!(!question_click_focuses(true, &surfaces, &body_click));

    let mut real_modal = FrameSurfaces::new();
    real_modal.push(SurfaceFrame::fixed(
        SurfaceId::ModalBody,
        Rect::new(10, 5, 20, 4),
    ));
    assert!(!question_click_focuses(false, &real_modal, &message_click));
    assert!(!question_click_focuses(false, &real_modal, &body_click));
}

/// A surface that scrolls its own rows still gets highlighted, but its
/// text is not read back from the frame: most of the selection is off it,
/// and the surface's own row cache is what holds those rows.
#[test]
fn a_scrollable_surface_is_highlighted_without_stashing_frame_text() {
    let mut terminal = Terminal::new(TestBackend::new(20, 6)).expect("terminal");
    let mut surfaces = FrameSurfaces::new();
    surfaces.push(SurfaceFrame::scrollable(
        SurfaceId::Transcript,
        Rect::new(0, 1, 20, 3),
        400,
        9_000,
    ));
    let mut selection = SelectionState::new();
    route_selection_event(
        &mut selection,
        &surfaces,
        mouse(MouseEventKind::Down(MouseButton::Left), 0, 1),
    );
    route_selection_event(
        &mut selection,
        &surfaces,
        mouse(MouseEventKind::Up(MouseButton::Left), 19, 3),
    );

    let mut text = Some("stale".to_owned());
    terminal
        .draw(|frame| {
            frame.render_widget(
                ratatui::widgets::Paragraph::new("visible transcript row"),
                Rect::new(0, 1, 20, 3),
            );
            text = draw_selection(frame, &selection, &surfaces);
        })
        .expect("draw");

    assert_eq!(text, None, "the extraction is the transcript's own job");
    assert_eq!(reversed_cells(&terminal).len(), 60, "all three rows lit up");
}

#[test]
fn escape_clears_a_finished_selection_before_the_view_sees_it() {
    let mut surfaces = FrameSurfaces::new();
    surfaces.push(SurfaceFrame::fixed(
        SurfaceId::ModalBody,
        Rect::new(10, 5, 20, 4),
    ));
    let mut selection = SelectionState::new();
    route_selection_event(
        &mut selection,
        &surfaces,
        mouse(MouseEventKind::Down(MouseButton::Left), 11, 6),
    );
    route_selection_event(
        &mut selection,
        &surfaces,
        mouse(MouseEventKind::Up(MouseButton::Left), 15, 7),
    );
    assert!(selection.range().is_some(), "the drag left a selection");

    assert_eq!(
        route_selection_event(&mut selection, &surfaces, escape()),
        SelectionRouting::Consumed
    );
    assert_eq!(selection.range(), None);
    // With nothing selected, Esc is the view's key again.
    assert_eq!(
        route_selection_event(&mut selection, &surfaces, escape()),
        SelectionRouting::Forward(escape())
    );
}

/// The dashboard loop batches buffered input and stops at the first event
/// that asks for work, so events that only need a redraw must report no
/// action and actionable keys must report theirs.
#[test]
fn only_events_that_ask_for_work_end_an_input_batch() {
    let mut dashboard = DashboardState::new(
        Config::default(),
        State::default(),
        std::collections::BTreeMap::new(),
    );

    assert!(matches!(
        dashboard_event_action(&mut dashboard, Event::Resize(80, 24)),
        DashboardAction::None
    ));
    // Escape no longer quits the combined surface, so a paste stands in for
    // an event that asks the controller to do work.
    assert!(matches!(
        dashboard_event_action(
            &mut dashboard,
            Event::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('v'),
                crossterm::event::KeyModifiers::CONTROL,
            )),
        ),
        DashboardAction::PasteFromClipboard
    ));
}

/// The default prefix key exactly as a terminal delivers it.
fn prefix_key() -> crossterm::event::KeyEvent {
    crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('b'),
        crossterm::event::KeyModifiers::CONTROL,
    )
}

/// Drives the prefix router the way the event loop does and reports what it
/// decided about the last key. A command that cannot run right now becomes
/// `Consumed`, exactly as the loop drops it.
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

/// The prefix chord whose second key is `character`.
fn chord(character: char) -> [crossterm::event::KeyEvent; 2] {
    [
        prefix_key(),
        plain_key(crossterm::event::KeyCode::Char(character)),
    ]
}

/// What a chord runs, or `None` when the router swallowed or forwarded it.
fn chord_command(
    dashboard: &mut DashboardState,
    keys: &[crossterm::event::KeyEvent],
) -> Option<CommandId> {
    match route(dashboard, keys) {
        KeyRoute::Command { id, .. } => Some(id),
        _ => None,
    }
}

fn plain_key(code: crossterm::event::KeyCode) -> crossterm::event::KeyEvent {
    crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
}

/// Walks the focus ring to `wanted`, which is the only way in from
/// outside the crate that owns the panes.
fn focus_on(dashboard: &mut DashboardState, wanted: mj_tui::Focus) {
    dashboard.focus_sessions();
    for _ in 0..8 {
        if dashboard.focus() == wanted {
            return;
        }
        dashboard.cycle_focus(false);
    }
    panic!("{wanted:?} is not on the focus ring");
}

/// The point of the chord: the user does not have to leave the composer
/// to start a session.
#[test]
fn the_create_chord_opens_the_wizard_while_the_composer_has_focus() {
    let mut dashboard = populated_dashboard();
    dashboard.focus_prompt();

    let command = chord_command(&mut dashboard, &chord('c')).expect("the create chord is bound");
    assert_eq!(command, CommandId::NewSessionWizard);
    assert_eq!(dashboard.dispatch_command(command), DashboardAction::None);
    assert!(dashboard.modal_open(), "New opens the creation wizard");
}

/// tmux's rule, and the one thing the prefix takes away: pressing `ctrl+b`
/// twice forwards the literal key, so the composer still moves the cursor
/// back one character.
#[test]
fn the_literal_prefix_reaches_the_composer_as_backward_char() {
    let mut dashboard = populated_dashboard();
    dashboard.focus_prompt();
    assert_eq!(route(&mut dashboard, &[prefix_key()]), KeyRoute::Consumed);
    assert!(dashboard.prefix_pending());
    assert_eq!(route(&mut dashboard, &[prefix_key()]), KeyRoute::Forward);
    assert!(!dashboard.prefix_pending());
}

/// A conversation's own modal owns the keyboard, except for the handful of
/// commands that cannot disturb it.
#[test]
fn a_bound_key_is_dropped_while_a_chat_modal_is_open_unless_it_survives_modals() {
    for id in [
        CommandId::Help,
        CommandId::QuitDetach,
        CommandId::TogglePanePreset,
        CommandId::Refresh,
    ] {
        assert!(mj_tui::survives_chat_modal(id), "{id:?}");
    }
    for id in [
        CommandId::NewSessionWizard,
        CommandId::ResumeDialog,
        CommandId::WebViewer,
        CommandId::OpenConfig,
    ] {
        assert!(!mj_tui::survives_chat_modal(id), "{id:?}");
    }
}

/// One key refreshes both support panes, from wherever the keyboard is —
/// including the composer, and including over an open dialog, because
/// asking for fresh figures cannot disturb what is on screen.
#[test]
fn the_refresh_chord_refreshes_targets_and_quotas_from_the_composer() {
    let mut dashboard = populated_dashboard();
    dashboard.focus_prompt();

    let command = chord_command(&mut dashboard, &chord('R')).expect("the refresh chord is bound");
    assert_eq!(command, CommandId::Refresh);
    assert!(matches!(
        dashboard.dispatch_command(command),
        DashboardAction::RefreshAll
    ));

    dashboard.dispatch_command(CommandId::Help);
    assert!(dashboard.modal_open());
    assert_eq!(
        chord_command(&mut dashboard, &chord('R')),
        Some(CommandId::Refresh),
        "refreshing is allowed over a modal"
    );
}

#[test]
fn the_pane_chords_cycle_focus_forward_and_backward() {
    let mut dashboard = populated_dashboard();
    dashboard.focus_sessions();
    let forward = [prefix_key(), plain_key(crossterm::event::KeyCode::Tab)];
    let command = chord_command(&mut dashboard, &forward).expect("the next-pane chord is bound");
    assert_eq!(command, CommandId::CycleFocus);
    dashboard.dispatch_command(command);
    assert_eq!(dashboard.focus(), mj_tui::Focus::Prompt);

    let reverse = [prefix_key(), plain_key(crossterm::event::KeyCode::BackTab)];
    let command =
        chord_command(&mut dashboard, &reverse).expect("the previous-pane chord is bound");
    assert_eq!(command, CommandId::CycleFocusReverse);
    dashboard.dispatch_command(command);
    assert_eq!(dashboard.focus(), mj_tui::Focus::Sessions);
}

/// The rendering toggle left the composer for the registry, so it now runs
/// from a pane and lands on whichever conversation is on screen.
#[tokio::test]
async fn prefix_t_toggles_rendering_of_the_visible_chat_from_a_pane() {
    let mut dashboard = populated_dashboard();
    dashboard.focus_sessions();

    let command = chord_command(&mut dashboard, &chord('t')).expect("the rendering chord is bound");
    assert_eq!(command, CommandId::ToggleTranscriptRendering);
    assert!(matches!(
        dashboard.dispatch_command(command),
        DashboardAction::ToggleTranscriptRendering
    ));

    let notices = Notices::default();
    let mut chat = open_test_chat_with_notices("rendering-toggle", notices.clone());
    super::actions::apply_chat_toggle(
        &mut dashboard,
        Some(&mut chat),
        super::actions::ChatToggle::TranscriptRendering,
    );
    assert_eq!(
        notices.current().as_deref(),
        Some("Raw transcript source enabled")
    );
    super::actions::apply_chat_toggle(
        &mut dashboard,
        Some(&mut chat),
        super::actions::ChatToggle::TranscriptRendering,
    );
    assert_eq!(
        notices.current().as_deref(),
        Some("Rich transcript rendering enabled")
    );

    // With nothing on screen the command explains itself rather than doing
    // nothing at all.
    super::actions::apply_chat_toggle(
        &mut dashboard,
        None,
        super::actions::ChatToggle::TranscriptRendering,
    );
    assert_eq!(
        dashboard.notice().as_deref(),
        Some("No conversation is open.")
    );
}

/// Resume is a chord like new session: the pane letter it used to answer
/// is gone, so this is the only way in from the composer.
#[test]
fn the_resume_chord_opens_the_resume_dialog_from_the_composer() {
    let mut dashboard = populated_dashboard();
    dashboard.focus_prompt();

    let command = chord_command(&mut dashboard, &chord('g')).expect("the resume chord is bound");
    assert_eq!(command, CommandId::ResumeDialog);
    assert!(matches!(
        dashboard.dispatch_command(command),
        DashboardAction::OpenResumeDialog
    ));
    assert_eq!(dashboard.focus(), mj_tui::Focus::Prompt);

    // Like the create chord, it waits for an open dialog to close.
    dashboard.show_resume_dialog(1, Vec::new());
    assert!(dashboard.modal_open());
    assert_eq!(chord_command(&mut dashboard, &chord('g')), None);
}

/// A chord that would act on a surface the user cannot see waits for the
/// dialog to close.
#[test]
fn the_create_chord_is_ignored_while_a_modal_is_open() {
    let mut dashboard = populated_dashboard();
    dashboard.focus_prompt();
    assert!(chord_command(&mut dashboard, &chord('c')).is_some());
    dashboard.dispatch_command(CommandId::NewSessionWizard);
    assert!(dashboard.modal_open());

    assert_eq!(chord_command(&mut dashboard, &chord('c')), None);
}

#[tokio::test]
async fn advertised_web_and_setup_shortcuts_open_their_dialogs_from_every_pane() {
    for focus in [
        mj_tui::Focus::Workspaces,
        mj_tui::Focus::Prompt,
        mj_tui::Focus::Sessions,
        mj_tui::Focus::Targets,
        mj_tui::Focus::Quota,
    ] {
        let mut dashboard = populated_dashboard();
        focus_on(&mut dashboard, focus);
        let fixture = mj_client::session::replacement_session_test_fixture("session-1", 1);
        let notices = Notices::default();
        let chat = ActiveChat::open(
            fixture.stopped,
            "bundle-1",
            None,
            fixture.control,
            SessionHeaderIdentity::default(),
            String::new(),
            notices.clone(),
        );
        // The fixture has no database to restore a review from; expose
        // the hints rather than the resulting startup notice.
        notices.clear();
        let mut chats = focused_chats(&mut dashboard, chat);
        let mut terminal = Terminal::new(TestBackend::new(240, 40)).expect("terminal");
        terminal
            .draw(|frame| {
                render_combined(frame, &mut dashboard, &mut chats, &BTreeMap::new(), false);
            })
            .expect("draw combined surface");
        let buffer = terminal.backend().buffer();
        let footer = (0..buffer.area.width)
            .map(|x| buffer[(x, buffer.area.bottom() - 1)].symbol())
            .collect::<String>();
        assert!(footer.contains("u web"), "{focus:?}: {footer}");
        assert!(footer.contains("s settings"), "{focus:?}: {footer}");

        let web = chord_command(&mut dashboard, &chord('u')).expect("the web chord is bound");
        assert_eq!(
            dashboard.dispatch_command(web),
            DashboardAction::LoadWebAccess
        );
        assert!(dashboard.modal_open());
        assert_eq!(chord_command(&mut dashboard, &chord('s')), None);
        dashboard.cancel_modal();

        let setup =
            chord_command(&mut dashboard, &chord('s')).expect("the settings chord is bound");
        assert_eq!(dashboard.dispatch_command(setup), DashboardAction::None);
        assert!(dashboard.modal_open());
        terminal
            .draw(|frame| {
                render_combined(frame, &mut dashboard, &mut chats, &BTreeMap::new(), false);
            })
            .expect("draw Setup");
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("Settings"), "{focus:?}: {screen}");
        assert!(screen.contains("Save and Close"), "{focus:?}: {screen}");
        assert_eq!(chord_command(&mut dashboard, &chord('u')), None);
    }
}

#[test]
fn workspace_tab_chords_reach_the_local_filter_from_the_composer() {
    let mut dashboard = populated_dashboard();
    let original = dashboard.active_workspace_id().unwrap().to_owned();
    dashboard.set_workspace_names(BTreeMap::from([
        (original.clone(), "Current".into()),
        ("other".into(), "Other".into()),
    ]));
    dashboard.focus_prompt();
    let keys = chord('n');
    let command =
        chord_command(&mut dashboard, &keys).expect("workspace tab shortcut reaches dashboard");
    assert!(
        matches!(dashboard.dispatch_command(command), DashboardAction::SelectWorkspace { workspace_id } if workspace_id == "other")
    );
    assert_eq!(
        dashboard.active_workspace_id(),
        Some(original.as_str()),
        "the controller captures drafts before changing the local filter"
    );
    dashboard.begin_workspace_manager();
    assert!(
        chord_command(&mut dashboard, &keys).is_none(),
        "tab shortcuts do not escape the modal"
    );
}

/// Every function key and every Alt letter left the defaults with the prefix,
/// so none of them may still run a command.
#[test]
fn function_keys_and_alt_letters_are_no_longer_bound() {
    for focus in [
        mj_tui::Focus::Sessions,
        mj_tui::Focus::Prompt,
        mj_tui::Focus::Targets,
        mj_tui::Focus::Quota,
    ] {
        let mut dashboard = populated_dashboard();
        focus_on(&mut dashboard, focus);
        for number in 1..=12 {
            assert_eq!(
                route(
                    &mut dashboard,
                    &[plain_key(crossterm::event::KeyCode::F(number))]
                ),
                KeyRoute::Forward,
                "{focus:?}: F{number}"
            );
        }
        for character in ['n', 's', 'a', 'g', 'q', 'w', 'x', 'z'] {
            assert_eq!(
                route(
                    &mut dashboard,
                    &[crossterm::event::KeyEvent::new(
                        crossterm::event::KeyCode::Char(character),
                        crossterm::event::KeyModifiers::ALT
                    )]
                ),
                KeyRoute::Forward,
                "{focus:?}: Alt-{character}"
            );
        }
    }
}

#[test]
fn the_read_chord_marks_all_read_from_the_targets_pane() {
    let mut dashboard = populated_dashboard();
    focus_on(&mut dashboard, mj_tui::Focus::Targets);

    let command = chord_command(&mut dashboard, &chord('a')).expect("the read chord is bound");
    assert_eq!(command, CommandId::MarkAllRead);
    dashboard.dispatch_command(command);
    // Nothing here is unread, and saying so is how the command reports it
    // ran from a pane that has no `a` of its own.
    assert_eq!(dashboard.notice().as_deref(), Some("No unread sessions."));
}

#[test]
fn the_cancel_chord_cancels_the_selected_sessions_launch_from_the_composer() {
    let mut dashboard = populated_dashboard();
    dashboard.focus_sessions();
    let session_id = dashboard
        .selected_session_id()
        .expect("a session is selected")
        .to_owned();
    dashboard.begin_session_operation(session_id.clone(), SessionOperationKind::Launching, None);
    dashboard.focus_prompt();

    let command = chord_command(&mut dashboard, &chord('C')).expect("the cancel chord is bound");
    assert_eq!(command, CommandId::CancelOperation);
    assert_eq!(
        dashboard.dispatch_command(command),
        DashboardAction::CancelOperation {
            session_id,
            kind: SessionOperationKind::Launching,
        }
    );
}

/// The cancel chord is allowed through exactly one modal: inside the
/// target-actions dialog it cancels the test that dialog is running.
#[test]
fn the_cancel_chord_inside_the_target_dialog_cancels_the_running_test() {
    let mut dashboard = populated_dashboard();
    focus_on(&mut dashboard, mj_tui::Focus::Targets);
    assert!(matches!(
        dashboard.handle_key(plain_key(crossterm::event::KeyCode::Enter)),
        DashboardAction::None
    ));
    assert!(dashboard.modal_open(), "the target actions dialog is open");
    // The target list is one Tab stop before Rename and Test.
    dashboard.handle_key(plain_key(crossterm::event::KeyCode::Tab));
    dashboard.handle_key(plain_key(crossterm::event::KeyCode::Tab));
    assert!(matches!(
        dashboard.handle_key(plain_key(crossterm::event::KeyCode::Enter)),
        DashboardAction::TestTarget { .. }
    ));

    let command = chord_command(&mut dashboard, &chord('C'))
        .expect("cancel reaches the dialog's running test");
    assert_eq!(command, CommandId::CancelOperation);
    assert!(matches!(
        dashboard.dispatch_command(command),
        DashboardAction::CancelTargetTest
    ));
    // With the test cancelled the dialog is an ordinary modal again, and
    // cancel waits for it to close.
    assert_eq!(chord_command(&mut dashboard, &chord('C')), None);
}

#[test]
fn plain_x_no_longer_cancels_anything() {
    let mut dashboard = populated_dashboard();
    dashboard.focus_sessions();
    let session_id = dashboard
        .selected_session_id()
        .expect("a session is selected")
        .to_owned();
    dashboard.begin_session_operation(session_id, SessionOperationKind::Launching, None);

    let plain_x = plain_key(crossterm::event::KeyCode::Char('x'));
    assert_eq!(chord_command(&mut dashboard, &[plain_x]), None);
    assert!(matches!(
        dashboard.handle_key(plain_x),
        DashboardAction::None
    ));
}

fn live_session(id: &str, created_at: &str) -> mj_core::state::SessionRecord {
    mj_core::state::SessionRecord {
        build_cache: None,
        container_workspace: None,
        mjolnir_subagents: None,
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

/// The conversation worth opening on is the one whose agent spoke most
/// recently, which is what the stored summaries record.
#[test]
fn startup_opens_the_session_with_the_newest_materialized_activity() {
    let sessions = [
        live_session("session-a", "2026-08-01T00:00:00Z"),
        live_session("session-b", "2026-08-02T00:00:00Z"),
        live_session("session-c", "2026-08-03T00:00:00Z"),
    ];
    let activity = |id: &str| match id {
        "session-a" => Some(10),
        "session-b" => Some(300),
        "session-c" => Some(200),
        _ => None,
    };

    assert_eq!(
        startup_session_choice(
            Some(mj_core::workspace::DEFAULT_WORKSPACE_ID),
            sessions.iter(),
            activity
        ),
        Some("session-b".into())
    );
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

#[test]
fn a_workspace_with_no_live_session_has_nothing_to_open() {
    assert_eq!(
        startup_session_choice(
            Some(mj_core::workspace::DEFAULT_WORKSPACE_ID),
            std::iter::empty(),
            |_| Some(1)
        ),
        None
    );
}

#[test]
fn startup_filters_global_records_without_creating_a_session_for_an_empty_workspace() {
    let mut active = live_session("active", "2026-08-01T00:00:00Z");
    active.workspace_id = "a".into();
    let mut archived = active.clone();
    archived.id = "archived".into();
    archived.archived = true;
    archived.workspace_id = "b".into();
    let mut stopped = archived.clone();
    stopped.id = "stopped".into();
    stopped.archived = false;
    stopped.state = mj_core::state::SessionState::Stopped;
    let records = [active, archived, stopped];
    assert_eq!(
        startup_session_choice(Some("b"), records.iter(), |_| Some(99)),
        Some("archived".into())
    );
    assert_eq!(
        startup_session_choice(None, records.iter(), |_| Some(99)),
        None
    );
    assert_eq!(
        startup_session_choice(Some("a"), records.iter(), |_| Some(99)),
        Some("active".into())
    );
    let mut startup = StartupSession::begin(std::iter::empty(), std::time::Instant::now());
    assert!(!startup.ready(std::time::Instant::now() + STARTUP_SESSION_WAIT));
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

/// Nothing may open a conversation on the surface's behalf until the pick has
/// run. `follow_selected_session` reads this: before the pick, the highlighted
/// row is only where the clamp left it, and following it would move the
/// keyboard out of the pane a restored arrangement named.
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

/// An empty workspace has nothing to wait for, so the surface never holds
/// the keyboard back from the Sessions pane.
#[test]
fn a_workspace_with_no_live_session_never_arms_the_startup_pick() {
    let start = std::time::Instant::now();
    let mut startup = StartupSession::begin(std::iter::empty(), start);
    assert!(!startup.ready(start));
    assert!(!startup.ready(start + STARTUP_SESSION_WAIT));
}

/// Two sessions in two panes, the keyboard in one of them: the split keys
/// have to make a pane. The selected conversation is the one this pane shows,
/// nothing came before it here, and every other session is on screen — so
/// there is nothing to move and the new pane starts empty rather than the key
/// refusing.
#[test]
fn splitting_with_every_session_on_screen_opens_an_empty_pane() {
    let left = PaneId::from_raw(2);
    let right = PaneId::from_raw(3);
    // The split that made these two panes cleared the focused pane's previous
    // session, which is why this case was unreachable before.
    assert_eq!(
        plan_split(right, Some(right), None),
        SplitPlan::EmptyNewPane
    );
    assert_eq!(plan_split(left, Some(left), None), SplitPlan::EmptyNewPane);
}

/// The other three outcomes, so the empty pane above is not swallowing them:
/// a session on screen elsewhere moves the keyboard, a session the selection
/// pulled into this pane moves to the new one, and a session that is not on
/// screen at all simply opens there.
#[test]
fn a_split_moves_the_keyboard_the_conversation_or_neither() {
    let focused = PaneId::from_raw(1);
    let other = PaneId::from_raw(4);
    assert_eq!(
        plan_split(focused, Some(other), None),
        SplitPlan::FocusPane(other)
    );
    assert_eq!(
        plan_split(focused, Some(focused), Some("before")),
        SplitPlan::MoveConversation {
            restore: "before".to_owned()
        }
    );
    assert_eq!(
        plan_split(focused, None, Some("before")),
        SplitPlan::OpenInNewPane
    );
}

/// One notice covered three different outcomes and named none of them. Each
/// one now says why, in a sentence that starts with the session and fits the
/// readable width of the footer.
#[test]
fn each_split_outcome_that_is_not_obvious_says_why() {
    let session = "5c4d7bfdac2f4f0e";
    let focus = SplitPlan::FocusPane(PaneId::from_raw(2))
        .notice(session)
        .unwrap();
    let empty = SplitPlan::EmptyNewPane.notice(session).unwrap();
    assert_ne!(focus, empty);
    for notice in [&focus, &empty] {
        assert!(notice.starts_with("5c4d7bfd"), "{notice}");
        assert!(
            notice.chars().count() <= 80,
            "{} columns: {notice}",
            notice.chars().count()
        );
    }
    assert!(focus.contains("another pane"), "{focus}");
    assert!(empty.contains("the new pane is empty"), "{empty}");
    // Moving the conversation where the key asked for it is visible on screen.
    assert!(SplitPlan::OpenInNewPane.notice(session).is_none());
    assert!(
        SplitPlan::MoveConversation {
            restore: "before".to_owned()
        }
        .notice(session)
        .is_none()
    );
}

#[test]
fn resume_progress_explains_the_blocking_work() {
    assert_eq!(
        resume_progress_notice("0123456789", "codex-1", "podman"),
        "Preparing 01234567: verifying checkpoint, provisioning podman, and restoring codex-1…"
    );
}

#[test]
fn only_a_fully_empty_config_triggers_automatic_setup() {
    let mut config = mj_core::config::Config::default();
    assert!(configuration_needs_setup(&config));
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
                environment: std::collections::BTreeMap::new(),
                workspace_storage: Default::default(),
            },
        },
    );
    assert!(!configuration_needs_setup(&config));
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

#[test]
fn shutdown_cancels_process_owning_critical_operations() {
    let (tracker, _changed) = CriticalOperationTracker::new();
    let cancelled = Arc::new(AtomicBool::new(false));
    let _guard = tracker.begin_cancellable("checking repository", cancelled.clone());

    tracker.cancel_all();

    assert!(cancelled.load(Ordering::Acquire));
}

/// Every warm conversation is pumped on every loop iteration, not only the
/// one the focused pane shows. Each of these chats starts on a stopped
/// session handle and only reaches its replacement actor while it is being
/// pumped, so both reporting the reconnection proves both were driven.
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
        while !notices.iter().all(|notices| {
            notices
                .current()
                .is_some_and(|notice| notice.contains("Reconnected"))
        }) {
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
}
