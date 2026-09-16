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
    let quit = Event::Key(crossterm::event::KeyEvent::new(
        KeyCode::Char('q'),
        KeyModifiers::ALT,
    ));
    assert!(!opening_cancel_event(&quit, true, false));
    let dashboard = populated_dashboard();
    assert_eq!(
        global_chord_event(&dashboard, &quit),
        Some(CommandId::QuitDetach)
    );
}

fn open_test_chat(session_id: &str) -> ActiveChat {
    let fixture = mj_client::session::replacement_session_test_fixture(session_id, 1);
    ActiveChat::open(
        fixture.stopped,
        "bundle-1",
        None,
        fixture.control,
        SessionHeaderIdentity::default(),
        String::new(),
        Notices::default(),
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
            render_combined(frame, dashboard, None, false);
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
    let mut chat = ActiveChat::open(
        fixture.stopped,
        "bundle-1",
        None,
        fixture.control,
        SessionHeaderIdentity::default(),
        "select this prompt text".into(),
        notices.clone(),
    );
    notices.clear();
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("terminal");
    terminal
        .draw(|frame| render_combined(frame, &mut dashboard, Some(&mut chat), false))
        .unwrap();
    dashboard.take_render_changed();
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
        dashboard.take_render_changed(),
        "focus change requests a frame"
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
    // Escape no longer quits the combined surface, so refresh stands in
    // for an event that asks the controller to do work.
    assert!(matches!(
        dashboard_event_action(
            &mut dashboard,
            Event::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::F(5),
                crossterm::event::KeyModifiers::NONE,
            )),
        ),
        DashboardAction::RefreshAll
    ));
}

/// The global chords the controller answers before anything else sees
/// the key. These drive the same two calls the batching loop makes.
fn chord(dashboard: &DashboardState, key: crossterm::event::KeyEvent) -> Option<CommandId> {
    global_chord_event(dashboard, &Event::Key(key))
}

fn alt(character: char) -> crossterm::event::KeyEvent {
    crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char(character),
        crossterm::event::KeyModifiers::ALT,
    )
}

fn function_key(number: u8) -> crossterm::event::KeyEvent {
    plain_key(crossterm::event::KeyCode::F(number))
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
fn alt_n_opens_the_wizard_while_the_composer_has_focus() {
    let mut dashboard = populated_dashboard();
    dashboard.focus_prompt();

    let command = chord(&dashboard, alt('n')).expect("Alt-N is a global chord");
    assert_eq!(command, CommandId::NewSessionWizard);
    assert_eq!(dashboard.dispatch_command(command), DashboardAction::None);
    assert!(dashboard.modal_open(), "New opens the creation wizard");
}

/// One key refreshes both support panes, from wherever the keyboard is —
/// including the composer, and including over an open dialog, because
/// asking for fresh figures cannot disturb what is on screen.
#[test]
fn f5_refreshes_targets_and_quotas_from_the_composer() {
    let mut dashboard = populated_dashboard();
    dashboard.focus_prompt();

    let command = chord(&dashboard, function_key(5)).expect("F5 is a global chord");
    assert_eq!(command, CommandId::Refresh);
    assert!(matches!(
        dashboard.dispatch_command(command),
        DashboardAction::RefreshAll
    ));

    dashboard.dispatch_command(CommandId::Help);
    assert!(dashboard.modal_open());
    assert_eq!(
        chord(&dashboard, function_key(5)),
        Some(CommandId::Refresh),
        "refreshing is allowed over a modal"
    );
}

#[test]
fn f6_global_path_cycles_forward_and_shift_f6_reverses_it() {
    let mut dashboard = populated_dashboard();
    dashboard.focus_sessions();
    let forward = Event::Key(function_key(6));
    let command = global_chord_event(&dashboard, &forward).expect("F6 is global");
    assert_eq!(command, CommandId::CycleFocus);
    assert!(apply_global_focus_cycle(&mut dashboard, &forward, command));
    assert_eq!(dashboard.focus(), mj_tui::Focus::Prompt);

    let reverse = Event::Key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::F(6),
        crossterm::event::KeyModifiers::SHIFT,
    ));
    let command = global_chord_event(&dashboard, &reverse).expect("Shift-F6 is global");
    assert_eq!(command, CommandId::CycleFocus);
    assert!(apply_global_focus_cycle(&mut dashboard, &reverse, command));
    assert_eq!(dashboard.focus(), mj_tui::Focus::Sessions);
}

/// Resume is a chord like new session: the pane letter it used to answer
/// is gone, so this is the only way in from the composer.
#[test]
fn alt_s_opens_the_resume_dialog_from_the_composer() {
    let mut dashboard = populated_dashboard();
    dashboard.focus_prompt();

    let command = chord(&dashboard, alt('s')).expect("Alt-S is a global chord");
    assert_eq!(command, CommandId::ResumeDialog);
    assert!(matches!(
        dashboard.dispatch_command(command),
        DashboardAction::OpenResumeDialog
    ));
    assert_eq!(dashboard.focus(), mj_tui::Focus::Prompt);

    // Like Alt-N, it waits for an open dialog to close.
    dashboard.show_resume_dialog(1, Vec::new());
    assert!(dashboard.modal_open());
    assert_eq!(chord(&dashboard, alt('s')), None);
}

/// A chord that would act on a surface the user cannot see waits for the
/// dialog to close.
#[test]
fn alt_n_is_ignored_while_a_modal_is_open() {
    let mut dashboard = populated_dashboard();
    dashboard.focus_prompt();
    assert!(chord(&dashboard, alt('n')).is_some());
    dashboard.dispatch_command(CommandId::NewSessionWizard);
    assert!(dashboard.modal_open());

    assert_eq!(chord(&dashboard, alt('n')), None);
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
        let mut chat = ActiveChat::open(
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
        let mut terminal = Terminal::new(TestBackend::new(240, 40)).expect("terminal");
        terminal
            .draw(|frame| render_combined(frame, &mut dashboard, Some(&mut chat), false))
            .expect("draw combined surface");
        let buffer = terminal.backend().buffer();
        let footer = (0..buffer.area.width)
            .map(|x| buffer[(x, buffer.area.bottom() - 1)].symbol())
            .collect::<String>();
        assert!(footer.contains("F4 web"), "{focus:?}: {footer}");
        assert!(footer.contains("F7 settings"), "{focus:?}: {footer}");

        let web = chord(&dashboard, function_key(4)).expect("F4 is global");
        assert_eq!(
            dashboard.dispatch_command(web),
            DashboardAction::LoadWebAccess
        );
        assert!(dashboard.modal_open());
        assert_eq!(chord(&dashboard, function_key(7)), None);
        dashboard.cancel_modal();

        let setup = chord(&dashboard, function_key(7)).expect("F7 is global");
        assert_eq!(dashboard.dispatch_command(setup), DashboardAction::None);
        assert!(dashboard.modal_open());
        terminal
            .draw(|frame| render_combined(frame, &mut dashboard, Some(&mut chat), false))
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
        assert_eq!(chord(&dashboard, function_key(4)), None);
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
    let key = crossterm::event::KeyEvent::new(KeyCode::PageDown, KeyModifiers::CONTROL);
    let command = chord(&dashboard, key).expect("workspace tab shortcut reaches dashboard");
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
        chord(&dashboard, key).is_none(),
        "tab shortcuts do not escape the modal"
    );
}

#[test]
fn f3_is_no_longer_a_workspace_shortcut() {
    for focus in [
        mj_tui::Focus::Sessions,
        mj_tui::Focus::Prompt,
        mj_tui::Focus::Targets,
        mj_tui::Focus::Quota,
    ] {
        let mut dashboard = populated_dashboard();
        focus_on(&mut dashboard, focus);
        assert_eq!(chord(&dashboard, function_key(3)), None, "{focus:?}");
    }
}

#[test]
fn alt_a_marks_all_read_from_the_targets_pane() {
    let mut dashboard = populated_dashboard();
    focus_on(&mut dashboard, mj_tui::Focus::Targets);

    let command = chord(&dashboard, alt('a')).expect("Alt-A is a global chord");
    assert_eq!(command, CommandId::MarkAllRead);
    dashboard.dispatch_command(command);
    // Nothing here is unread, and saying so is how the command reports it
    // ran from a pane that has no `a` of its own.
    assert_eq!(dashboard.notice().as_deref(), Some("No unread sessions."));
}

#[test]
fn alt_x_cancels_the_selected_sessions_launch_from_the_composer() {
    let mut dashboard = populated_dashboard();
    dashboard.focus_sessions();
    let session_id = dashboard
        .selected_session_id()
        .expect("a session is selected")
        .to_owned();
    dashboard.begin_session_operation(session_id.clone(), SessionOperationKind::Launching, None);
    dashboard.focus_prompt();

    let command = chord(&dashboard, alt('x')).expect("Alt-X is a global chord");
    assert_eq!(command, CommandId::CancelOperation);
    assert_eq!(
        dashboard.dispatch_command(command),
        DashboardAction::CancelOperation {
            session_id,
            kind: SessionOperationKind::Launching,
        }
    );
}

/// Inside the target-actions dialog Alt-X belongs to the test that dialog
/// is running, so the pre-filter must leave the key alone.
#[test]
fn alt_x_inside_the_target_dialog_cancels_the_running_test() {
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

    assert_eq!(
        chord(&dashboard, alt('x')),
        None,
        "the dialog keeps the key"
    );
    assert!(matches!(
        dashboard.handle_key(alt('x')),
        DashboardAction::CancelTargetTest
    ));
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
    assert_eq!(chord(&dashboard, plain_x), None);
    assert!(matches!(
        dashboard.handle_key(plain_x),
        DashboardAction::None
    ));
}

fn live_session(id: &str, created_at: &str) -> mj_core::state::SessionRecord {
    mj_core::state::SessionRecord {
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
