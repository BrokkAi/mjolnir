use crossterm::event::KeyEvent;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Position;

use mj_core::state::SessionState;

use super::*;
use crate::test_support::*;

use crate::render::render;
use crate::{DashboardAction, DashboardState, Mode};

#[test]
fn remote_repair_requires_confirmation_and_restores_the_previous_screen() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.show_close_failure("session-1".into(), "prior dialog");
    let previous = dashboard.mode.clone();
    let repair = mj_core::local_git::LocalRemoteRepair {
        path: "/project".into(),
        branch: "main".into(),
        missing_remote: "upstream".into(),
        replacement_remote: "origin".into(),
        fetch_url: "https://example.com/repo.git".into(),
        push_urls: vec!["ssh://git@example.com/repo.git".into()],
    };
    let retry = DashboardAction::CreateSession {
        mjolnir_subagents: None,
        workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.into(),
        profile_id: "codex".into(),
        target_template_id: "docker".into(),
        bundle_id: "project".into(),
        project_directory: None,
        create_managed_worktree: Some(false),
        additional_mounts: Vec::new(),
        resource_allocation: None,
    };
    dashboard.show_remote_repair_confirmation("repo".into(), vec![repair.clone()], retry.clone());
    let mut terminal = Terminal::new(TestBackend::new(100, 25)).unwrap();
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.contains("Repair Git tracking?"));
    assert!(text.contains("ssh://git@example.com/repo.git"));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Esc)),
        DashboardAction::None
    );
    assert_eq!(dashboard.mode, previous);
    dashboard.show_remote_repair_confirmation("repo".into(), vec![repair.clone()], retry.clone());
    dashboard.handle_key(key(KeyCode::Right));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::RepairRepositoryRemotes {
            bundle_id: "repo".into(),
            repairs: vec![repair],
            retry: Box::new(retry),
        }
    );
    assert_eq!(dashboard.mode, previous);
}

#[test]
fn configuration_repair_can_open_setup_or_preserved_transcript() {
    let mut dashboard = dashboard_with_session(stopped_session());
    for choice in [1, 2] {
        let action = dashboard.activate_confirmation_button(
            Confirmation::ConfigurationRepair {
                session_id: "session-1".into(),
                error: "missing bundle".into(),
                previous: Box::new(Mode::Dashboard),
            },
            choice,
        );
        if choice == 1 {
            assert_eq!(
                action,
                DashboardAction::Open {
                    session_id: "session-1".into()
                }
            );
        } else {
            assert_eq!(action, DashboardAction::None);
            assert!(matches!(dashboard.mode, Mode::Setup(_)));
        }
    }
}

#[test]
fn launch_failure_survives_notices_and_retries_original_settings_once() {
    let mut dashboard = dashboard_with_session(stopped_session());
    let retry = DashboardAction::CreateSession {
        mjolnir_subagents: None,
        create_managed_worktree: None,
        workspace_id: "original-workspace".into(),
        profile_id: "codex".into(),
        bundle_id: "project".into(),
        project_directory: None,
        target_template_id: "docker".into(),
        additional_mounts: Vec::new(),
        resource_allocation: None,
    };
    dashboard.show_launch_failure("upload failed", Some(retry.clone()));
    dashboard.set_notice("Quota refreshed");
    let mut terminal = Terminal::new(TestBackend::new(90, 25)).unwrap();
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.contains("Launch failed"));
    assert!(text.contains("upload failed"));
    assert!(text.contains("Retry launch"));
    dashboard.handle_key(key(KeyCode::Right));
    assert_eq!(dashboard.handle_key(key(KeyCode::Enter)), retry);
    assert!(!matches!(dashboard.mode, Mode::Confirm(_)));
}

#[test]
fn launch_failure_scrolls_long_details_and_restores_interrupted_dialog() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.show_close_failure("session-1".into(), "prior error");
    let previous = dashboard.mode.clone();
    let error = (0..60)
        .map(|index| format!("diagnostic line {index}\n"))
        .collect::<String>();
    dashboard.show_launch_failure(error, None);
    let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .unwrap();
    for _ in 0..30 {
        dashboard.handle_key(key(KeyCode::PageDown));
    }
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.contains("diagnostic line 59"), "{text}");
    assert!(!text.contains("Retry launch"));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Esc)),
        DashboardAction::None
    );
    assert_eq!(dashboard.mode, previous);
}

#[test]
fn web_qr_has_a_four_module_quiet_zone() {
    let qr = render_qr("https://example.test/auth/login?token=secret").unwrap();
    let lines = qr.lines().collect::<Vec<_>>();
    assert!(lines.len() > 4);
    assert!(lines[0].chars().all(|character| character == ' '));
    assert!(lines[1].chars().all(|character| character == ' '));
    assert!(lines.iter().all(|line| line.starts_with("    ")));
    assert!(lines.iter().all(|line| line.ends_with("    ")));
}

fn draw_web_dialog(dialog: &WebDialog, width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal
        .draw(|frame| {
            let mut surfaces = FrameSurfaces::new();
            render_web_dialog(frame, frame.area(), dialog, &mut surfaces);
        })
        .expect("draw web dialog");
    buffer_lines(terminal.backend().buffer())
}

#[test]
fn web_dialog_without_a_qr_keeps_access_details_and_close_readable() {
    let mut dialog = WebDialog::loading();
    dialog.loading = false;
    dialog.viewer_url = Some("http://127.0.0.1:37650".to_owned());
    dialog.viewer_code = Some("022160".to_owned());
    dialog.fallback_reason = Some("automatic Tailscale detection is disabled".to_owned());
    let rendered = draw_web_dialog(&dialog, 140, 40).join("\n");
    assert!(rendered.contains("Web viewer"));
    assert!(rendered.contains("http://127.0.0.1:37650"));
    assert!(rendered.contains("Viewer code: 022160"));
    assert!(rendered.contains("× Web viewer"));
}

#[test]
fn web_dialog_wraps_a_long_url_without_truncating_it() {
    // A URL wider than the QR must wrap within the box, not get cut off.
    let url = "https://a-very-long-machine-name.some-tailnet.ts.net:37650/viewer";
    let dialog = WebDialog {
        loading: false,
        viewer_url: Some(url.to_owned()),
        viewer_code: Some("022160".to_owned()),
        fallback_reason: None,
        message: None,
        qr: Some(render_qr(url).unwrap()),
        ..WebDialog::loading()
    };

    let rendered = draw_web_dialog(&dialog, 60, 40);

    // Every box row fits the terminal, so the dialog never overflows.
    assert!(rendered.iter().all(|line| line.chars().count() <= 60));
    // The URL survives in full once the border padding is stripped away.
    // Drop whitespace and the box border so the URL's wrapped halves sit
    // adjacent, then confirm none of its characters were lost.
    let flat = rendered
        .join("")
        .chars()
        .filter(|character| !character.is_whitespace() && *character != '│')
        .collect::<String>();
    assert!(
        flat.contains(url),
        "the full URL should appear (wrapped) in the dialog"
    );
    assert!(
        rendered
            .iter()
            .any(|line| line.contains("Viewer code: 022160"))
    );
}

fn failed_web_dashboard() -> DashboardState {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.open_web_dialog();
    dashboard.apply_web_access(WebViewerAccess::Failed {
        address: "127.0.0.1:37650".parse().unwrap(),
        message: "Port 37650 is already in use.".into(),
        port_conflict: true,
    });
    dashboard
}

fn activate_web(dashboard: &mut DashboardState, control: DialogControl) -> DashboardAction {
    let Mode::Web(mut dialog) = dashboard.mode.clone() else {
        panic!("web dialog expected")
    };
    draw_web_dialog(&dialog, 80, 24);
    dialog.form.get_mut().focus(control);
    dashboard.handle_web_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        dialog,
    )
}

#[test]
fn web_port_conflict_offers_recovery_and_keeps_the_address_and_dismiss_visible() {
    let dashboard = failed_web_dashboard();
    let Mode::Web(dialog) = &dashboard.mode else {
        unreachable!()
    };
    for (width, height) in [(140, 40), (80, 24), (60, 20)] {
        let rendered = draw_web_dialog(dialog, width, height);
        let text = rendered.join("\n");
        assert!(text.contains("Port 37650 is already in use."));
        assert!(text.contains("Address: 127.0.0.1:37650"));
        assert!(text.contains("  Use another port  "));
        assert!(text.contains("  Inspect port  "));
        assert!(text.contains("  Retry  "));
        assert!(text.contains("× Web viewer"));
        assert!(
            rendered
                .iter()
                .all(|line| line.trim().chars().count() <= 62)
        );
    }
}

#[test]
fn web_recovery_enters_loading_immediately_and_dismiss_cancels_status_polling() {
    let mut dashboard = failed_web_dashboard();
    assert_eq!(
        activate_web(&mut dashboard, DialogControl::WebAnotherPort),
        DashboardAction::RecoverWebViewer(WebViewerRecovery::AnotherPort)
    );
    let Mode::Web(dialog) = &dashboard.mode else {
        unreachable!()
    };
    assert!(dialog.loading);
    let rendered = draw_web_dialog(dialog, 80, 24).join("\n");
    assert!(rendered.contains("Starting web viewer"));
    assert!(rendered.contains("× Web viewer"));
    assert!(!rendered.contains("  Use another port  "));
    assert_eq!(
        dashboard.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        DashboardAction::CancelWebAccess
    );
    dashboard.apply_web_access(WebViewerAccess::Starting);
    assert!(
        !matches!(dashboard.mode, Mode::Web(_)),
        "late results must not reopen the dialog"
    );
}

#[test]
fn web_retry_and_inspection_dispatch_real_actions_and_inspection_can_fail_in_place() {
    let mut dashboard = failed_web_dashboard();
    assert_eq!(
        activate_web(&mut dashboard, DialogControl::WebRetry),
        DashboardAction::RecoverWebViewer(WebViewerRecovery::Retry)
    );
    dashboard = failed_web_dashboard();
    assert_eq!(
        activate_web(&mut dashboard, DialogControl::WebInspect),
        DashboardAction::InspectWebListener
    );
    let Mode::Web(dialog) = &dashboard.mode else {
        unreachable!()
    };
    assert!(dialog.inspecting);
    dashboard.apply_web_listeners(Err("Permission denied while inspecting listeners.".into()));
    let Mode::Web(dialog) = &dashboard.mode else {
        unreachable!()
    };
    assert!(!dialog.inspecting);
    let rendered = draw_web_dialog(dialog, 80, 24).join("\n");
    assert!(rendered.contains("Permission denied"));
    assert!(rendered.contains("  Use another port  "));
}

#[test]
fn stopping_a_web_listener_requires_a_separate_confirmation_with_cancel_selected() {
    let mut dashboard = failed_web_dashboard();
    let process = WebListenerProcess {
        pid: 4242,
        name: "mj".into(),
        executable: "/opt/mj/bin/mj".into(),
        started_at: 123,
        stop_disabled_reason: None,
    };
    dashboard.apply_web_listeners(Ok(vec![process.clone()]));
    assert_eq!(
        activate_web(&mut dashboard, DialogControl::WebStop),
        DashboardAction::None
    );
    let Mode::Web(dialog) = &dashboard.mode else {
        unreachable!()
    };
    assert_eq!(
        dialog.form.borrow().focused(),
        Some(DialogControl::WebCancelStop)
    );
    let rendered = draw_web_dialog(dialog, 80, 24).join("\n");
    assert!(rendered.contains("PID 4242"));
    assert!(rendered.contains("/opt/mj/bin/mj"));
    assert!(rendered.contains("  Stop and retry  "));
    assert_eq!(
        activate_web(&mut dashboard, DialogControl::WebCancelStop),
        DashboardAction::None
    );
    let Mode::Web(dialog) = &dashboard.mode else {
        unreachable!()
    };
    assert!(dialog.confirm_stop.is_none());
    activate_web(&mut dashboard, DialogControl::WebStop);
    assert_eq!(
        activate_web(&mut dashboard, DialogControl::WebConfirmStop),
        DashboardAction::RecoverWebViewer(WebViewerRecovery::StopAndRetry(process))
    );
}

#[test]
fn an_unrelated_listener_has_no_enabled_stop_action() {
    let mut dashboard = failed_web_dashboard();
    dashboard.apply_web_listeners(Ok(vec![WebListenerProcess {
        pid: 4242,
        name: "other-server".into(),
        executable: "/opt/other-server".into(),
        started_at: 123,
        stop_disabled_reason: Some("This is not an identified Mjolnir server.".into()),
    }]));
    let Mode::Web(dialog) = &dashboard.mode else {
        unreachable!()
    };
    let rendered = draw_web_dialog(dialog, 80, 24);
    let (row, line) = rendered
        .iter()
        .enumerate()
        .find(|(_, line)| line.contains("  Stop server…  "))
        .unwrap();
    let column = line.find("  Stop server…  ").unwrap();
    for kind in [
        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
    ] {
        let Mode::Web(dialog) = dashboard.mode.clone() else {
            unreachable!()
        };
        assert_eq!(
            dashboard.handle_web_event(
                Event::Mouse(crossterm::event::MouseEvent {
                    kind,
                    column: column as u16 + 2,
                    row: row as u16,
                    modifiers: KeyModifiers::NONE
                }),
                dialog
            ),
            DashboardAction::None
        );
    }
    let Mode::Web(dialog) = &dashboard.mode else {
        unreachable!()
    };
    assert!(dialog.confirm_stop.is_none());
}

fn dashboard_with_container_session() -> DashboardState {
    let mut session = running_session();
    session.additional_mounts = vec![AdditionalMount {
        source: PathBuf::from("/srv/data"),
        destination: PathBuf::from("/mnt/data"),
        read_only: false,
    }];
    let mut dashboard = dashboard_with_session(session);
    dashboard
        .state
        .mount_history
        .insert("local".into(), vec![PathBuf::from("/srv/models")]);
    dashboard
}

fn container_editor(dashboard: &DashboardState) -> &ContainerEditor {
    let Mode::EditContainer(editor) = &dashboard.mode else {
        panic!("expected the container editor");
    };
    editor
}

/// Reaches a session command the way the user does now: `F2`, type
/// enough of the name to pick it out, Enter. The session edit dialog
/// these fixtures used to press `e` for no longer exists.
fn through_the_palette(dashboard: &mut DashboardState, query: &str) {
    dashboard.handle_key(key(KeyCode::F(2)));
    assert!(
        matches!(dashboard.mode, Mode::Palette(_)),
        "F2 opens the palette"
    );
    for character in query.chars() {
        dashboard.handle_key(key(KeyCode::Char(character)));
    }
    dashboard.handle_key(key(KeyCode::Enter));
}

fn open_container_editor(dashboard: &mut DashboardState) {
    through_the_palette(dashboard, "container");
    assert!(matches!(dashboard.mode, Mode::EditContainer(_)));
}

fn open_rename_editor(dashboard: &mut DashboardState) {
    through_the_palette(dashboard, "rename");
    assert!(matches!(dashboard.mode, Mode::Rename(_)));
}

#[test]
fn setup_opens_in_place_and_container_settings_remain_available() {
    let mut empty = DashboardState::new(
        mj_core::config::Config {
            subagents: Default::default(),
            version: mj_core::config::CONFIG_VERSION,
            sessions_side: Default::default(),
            advanced: Default::default(),
            show_stopped_sessions: false,
            spinner: Default::default(),
            theme: Default::default(),
            phone: Default::default(),
            review: Default::default(),
            legacy_startup: (),
            profiles: Default::default(),
            bundles: Default::default(),
            targets: Default::default(),
        },
        mj_core::state::State::default(),
        Default::default(),
    );
    assert_eq!(
        empty.handle_key(key(KeyCode::Char('e'))),
        DashboardAction::None
    );
    assert!(matches!(empty.mode, Mode::Setup(_)));

    let mut dashboard = dashboard_with_container_session();
    open_container_editor(&mut dashboard);
    let editor = container_editor(&dashboard);
    assert_eq!(editor.session_id, "session-1");
    assert_eq!(editor.mounts.len(), 1);
    assert_eq!(editor.suggestions, vec![PathBuf::from("/srv/models")]);
}

#[test]
fn container_editor_saves_edited_size_mounts_and_remembered_sources() {
    let mut dashboard = dashboard_with_container_session();
    open_container_editor(&mut dashboard);
    for character in "4".chars() {
        dashboard.handle_key(key(KeyCode::Char(character)));
    }
    dashboard.handle_key(key(KeyCode::Tab));
    for character in "6g".chars() {
        dashboard.handle_key(key(KeyCode::Char(character)));
    }
    assert_eq!(
        container_editor(&dashboard).focused(),
        ContainerEditFocus::Memory
    );

    // Take the remembered directory as the next mount.
    while container_editor(&dashboard).focused() != ContainerEditFocus::Suggestions {
        dashboard.handle_key(key(KeyCode::Tab));
    }
    dashboard.handle_key(key(KeyCode::Enter));
    assert_eq!(container_editor(&dashboard).source, "/srv/models");
    dashboard.handle_key(key(KeyCode::Enter));
    assert_eq!(
        container_editor(&dashboard).mounts,
        vec![
            AdditionalMount {
                source: PathBuf::from("/srv/data"),
                destination: PathBuf::from("/mnt/data"),
                read_only: false,
            },
            AdditionalMount {
                source: PathBuf::from("/srv/models"),
                destination: PathBuf::from("/mnt/models"),
                read_only: false,
            },
        ]
    );

    // Forget the remembered directory, then drop the original mount.
    while container_editor(&dashboard).focused() != ContainerEditFocus::Suggestions {
        dashboard.handle_key(key(KeyCode::Tab));
    }
    dashboard.handle_key(key(KeyCode::Char('d')));
    assert!(container_editor(&dashboard).suggestions.is_empty());
    while container_editor(&dashboard).focused() != ContainerEditFocus::Mounts {
        dashboard.handle_key(key(KeyCode::Tab));
    }
    dashboard.handle_key(key(KeyCode::Up));
    assert_eq!(container_editor(&dashboard).mount_index, 0);
    dashboard.handle_key(key(KeyCode::Char('d')));

    while container_editor(&dashboard).focused() != ContainerEditFocus::Save {
        dashboard.handle_key(key(KeyCode::Tab));
    }
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::SaveContainerSettings {
            session_id: "session-1".into(),
            cpus: Some("4".into()),
            memory: Some("6g".into()),
            additional_mounts: vec![AdditionalMount {
                source: PathBuf::from("/srv/models"),
                destination: PathBuf::from("/mnt/models"),
                read_only: false,
            }],
            mount_history: Vec::new(),
        }
    );
    assert!(matches!(dashboard.mode, Mode::Dashboard));
}

#[test]
fn container_editor_marks_new_and_existing_mounts_read_only() {
    let mut dashboard = dashboard_with_container_session();
    open_container_editor(&mut dashboard);

    // Space on the checkbox attaches the next directory read-only.
    while container_editor(&dashboard).focused() != ContainerEditFocus::Source {
        dashboard.handle_key(key(KeyCode::Tab));
    }
    for character in "/nfs/share".chars() {
        dashboard.handle_key(key(KeyCode::Char(character)));
    }
    dashboard.handle_key(key(KeyCode::Tab));
    dashboard.handle_key(key(KeyCode::Tab));
    assert_eq!(
        container_editor(&dashboard).focused(),
        ContainerEditFocus::ReadOnly
    );
    dashboard.handle_key(key(KeyCode::Char(' ')));
    assert!(container_editor(&dashboard).read_only);
    while container_editor(&dashboard).focused() != ContainerEditFocus::Source {
        dashboard.handle_key(key(KeyCode::Tab));
    }
    dashboard.handle_key(key(KeyCode::Enter));

    // Space on a listed row toggles that row, and the flag is saved.
    while container_editor(&dashboard).focused() != ContainerEditFocus::Mounts {
        dashboard.handle_key(key(KeyCode::Tab));
    }
    dashboard.handle_key(key(KeyCode::Up));
    assert_eq!(container_editor(&dashboard).mount_index, 0);
    dashboard.handle_key(key(KeyCode::Char(' ')));

    while container_editor(&dashboard).focused() != ContainerEditFocus::Save {
        dashboard.handle_key(key(KeyCode::Tab));
    }
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::SaveContainerSettings {
            session_id: "session-1".into(),
            cpus: None,
            memory: None,
            additional_mounts: vec![
                AdditionalMount {
                    source: PathBuf::from("/srv/data"),
                    destination: PathBuf::from("/mnt/data"),
                    read_only: true,
                },
                AdditionalMount {
                    source: PathBuf::from("/nfs/share"),
                    destination: PathBuf::from("/mnt/share"),
                    read_only: true,
                },
            ],
            mount_history: vec![PathBuf::from("/srv/models")],
        }
    );
}

#[test]
fn container_editor_says_when_the_change_takes_effect() {
    let mut dashboard = dashboard_with_container_session();
    open_container_editor(&mut dashboard);
    let mut terminal = Terminal::new(TestBackend::new(100, 40)).expect("terminal");
    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .expect("draw editor");
    let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
    assert!(rendered.contains("Applies when the container is next recreated"));
    assert!(rendered.contains("/srv/data"));
}

#[test]
fn rename_uses_acp_title_as_the_initial_value() {
    let mut dashboard = dashboard_with_session(running_session());
    open_rename_editor(&mut dashboard);
    let Mode::Rename(editor) = &dashboard.mode else {
        panic!("expected rename editor");
    };
    assert_eq!(editor.form.borrow().focused(), Some(DialogControl::Field));
    for character in " v2".chars() {
        dashboard.handle_key(key(KeyCode::Char(character)));
    }
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::RenameSession {
            session_id: "session-1".into(),
            title: "ACP pretty name v2".into(),
        }
    );
    assert!(matches!(dashboard.mode, Mode::Dashboard));
}

#[test]
fn focused_text_fields_own_readline_keys_and_control_c_cancels() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.begin_rename();
    for character in " alpha beta".chars() {
        dashboard.handle_key(key(KeyCode::Char(character)));
    }

    let control_key = |character| KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL);
    assert_eq!(
        dashboard.handle_key(control_key('w')),
        DashboardAction::None
    );
    let Mode::Rename(editor) = &dashboard.mode else {
        panic!("expected rename editor");
    };
    assert!(editor.title.ends_with("alpha "));

    assert_eq!(
        dashboard.handle_key(control_key('c')),
        DashboardAction::None
    );
    assert!(dashboard.dialog_confirmation_open());
    dashboard.handle_key(key(KeyCode::Enter));
    assert!(!dashboard.dialog_confirmation_open());
    assert_eq!(rename_focus(&dashboard), DialogControl::Field);
    dashboard.handle_key(key(KeyCode::Esc));
    dashboard.handle_key(key(KeyCode::Right));
    dashboard.handle_key(key(KeyCode::Enter));
    assert!(matches!(dashboard.mode, Mode::Dashboard));
}

fn dashboard_with_rename_editor() -> DashboardState {
    let mut dashboard = dashboard_with_session(running_session());
    open_rename_editor(&mut dashboard);
    dashboard
}

fn rename_focus(dashboard: &DashboardState) -> DialogControl {
    let Mode::Rename(editor) = &dashboard.mode else {
        panic!("expected rename editor");
    };
    match editor.form.borrow().focused() {
        Some(DialogControl::Field) => DialogControl::Field,
        Some(DialogControl::Cancel) => DialogControl::Cancel,
        Some(DialogControl::Save) => DialogControl::Save,
        focused => panic!("unexpected rename focus: {focused:?}"),
    }
}

#[test]
fn rename_editor_cycles_focus_from_the_field_through_both_buttons() {
    let mut dashboard = dashboard_with_rename_editor();
    for expected in [
        DialogControl::Cancel,
        DialogControl::Save,
        DialogControl::Field,
        DialogControl::Cancel,
    ] {
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Tab)),
            DashboardAction::None
        );
        assert_eq!(rename_focus(&dashboard), expected);
    }

    let mut dashboard = dashboard_with_rename_editor();
    for expected in [
        DialogControl::Save,
        DialogControl::Cancel,
        DialogControl::Field,
    ] {
        assert_eq!(
            dashboard.handle_key(key(KeyCode::BackTab)),
            DashboardAction::None
        );
        assert_eq!(rename_focus(&dashboard), expected);
    }
}

#[test]
fn rename_editor_arrows_move_between_buttons_but_never_edit_the_field() {
    let mut dashboard = dashboard_with_rename_editor();
    // The field has no cursor, so arrows there change nothing.
    for arrow in [KeyCode::Left, KeyCode::Right] {
        assert_eq!(dashboard.handle_key(key(arrow)), DashboardAction::None);
        assert_eq!(rename_focus(&dashboard), DialogControl::Field);
    }

    dashboard.handle_key(key(KeyCode::Tab));
    assert_eq!(rename_focus(&dashboard), DialogControl::Cancel);
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Right)),
        DashboardAction::None
    );
    assert_eq!(rename_focus(&dashboard), DialogControl::Save);
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Left)),
        DashboardAction::None
    );
    assert_eq!(rename_focus(&dashboard), DialogControl::Cancel);
}

#[test]
fn rename_editor_buttons_ignore_typing_and_backspace() {
    let mut dashboard = dashboard_with_rename_editor();
    dashboard.handle_key(key(KeyCode::Tab));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Char('x'))),
        DashboardAction::None
    );
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Backspace)),
        DashboardAction::None
    );
    let Mode::Rename(editor) = &dashboard.mode else {
        panic!("expected rename editor");
    };
    assert_eq!(editor.title, "ACP pretty name");
    assert_eq!(editor.form.borrow().focused(), Some(DialogControl::Cancel));
}

#[test]
fn rename_editor_cancel_button_closes_without_renaming() {
    let mut dashboard = dashboard_with_rename_editor();
    dashboard.handle_key(key(KeyCode::Tab));
    assert_eq!(rename_focus(&dashboard), DialogControl::Cancel);
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert!(matches!(dashboard.mode, Mode::Dashboard));
}

#[test]
fn rename_editor_save_button_renames_like_the_field() {
    let mut dashboard = dashboard_with_rename_editor();
    dashboard.handle_key(key(KeyCode::Char('!')));
    dashboard.handle_key(key(KeyCode::Tab));
    dashboard.handle_key(key(KeyCode::Tab));
    assert_eq!(rename_focus(&dashboard), DialogControl::Save);
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::RenameSession {
            session_id: "session-1".into(),
            title: "ACP pretty name!".into(),
        }
    );
}

#[test]
fn rename_editor_rejects_an_empty_title_from_the_field_and_the_save_button() {
    for focus_moves in [0, 2] {
        let mut dashboard = dashboard_with_rename_editor();
        let Mode::Rename(editor) = &mut dashboard.mode else {
            panic!("expected rename editor");
        };
        editor.title.clear();
        for _ in 0..focus_moves {
            dashboard.handle_key(key(KeyCode::Tab));
        }
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::None,
            "{focus_moves} focus moves"
        );
        assert_eq!(
            dashboard.notice().as_deref(),
            Some("Session name cannot be empty."),
            "{focus_moves} focus moves"
        );
        assert!(matches!(dashboard.mode, Mode::Rename(_)), "{focus_moves}");
    }
}

#[test]
fn rename_editor_escape_cancels_from_any_focus() {
    for focus_moves in 0..3 {
        let mut dashboard = dashboard_with_rename_editor();
        for _ in 0..focus_moves {
            dashboard.handle_key(key(KeyCode::Tab));
        }
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Esc)),
            DashboardAction::None,
            "{focus_moves} focus moves"
        );
        assert!(matches!(dashboard.mode, Mode::Dashboard), "{focus_moves}");
    }
}

#[test]
fn rename_editor_highlights_save_until_cancel_takes_focus() {
    let mut dashboard = dashboard_with_rename_editor();
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");
    let mut button_styles = |dashboard: &mut DashboardState| {
        terminal
            .draw(|frame| render(frame, dashboard))
            .expect("draw rename editor");
        let buffer = terminal.backend().buffer();
        let lines = buffer_lines(buffer);
        let row = lines
            .iter()
            .position(|line| line.contains(" Cancel ") && line.contains(" Save "))
            .expect("button row");
        let y = buffer.area.y + row as u16;
        assert!(!lines.iter().any(|line| line.contains("Enter save")));
        (
            buffer[(buffer.area.x + cell_column(&lines[row], "Cancel"), y)].bg,
            buffer[(buffer.area.x + cell_column(&lines[row], "Save"), y)].bg,
        )
    };

    // The shared button row keeps both buttons in their normal style while
    // the field has focus. Tab then moves the accent focus style between the
    // footer buttons.
    assert_eq!(
        button_styles(&mut dashboard),
        (
            theme::palette().surface_raised,
            theme::palette().surface_raised
        )
    );

    dashboard.handle_key(key(KeyCode::Tab));
    assert_eq!(
        button_styles(&mut dashboard),
        (theme::palette().accent, theme::palette().surface_raised)
    );

    dashboard.handle_key(key(KeyCode::Tab));
    assert_eq!(
        button_styles(&mut dashboard),
        (theme::palette().surface_raised, theme::palette().accent)
    );
}

#[test]
fn import_progress_renders_a_focused_cancel_button_that_confirms_cancellation() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.show_import_progress("Chosen session".into());
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw import progress");
    let buffer = terminal.backend().buffer();
    let lines = buffer_lines(buffer);
    let row = lines
        .iter()
        .position(|line| line.contains(" Cancel "))
        .expect("button row");
    let y = buffer.area.y + row as u16;
    let cancel_x = buffer.area.x + cell_column(&lines[row], "Cancel");
    assert_eq!(buffer[(cancel_x, y)].bg, theme::palette().accent);
    assert_eq!(buffer[(cancel_x - 1, y)].bg, theme::palette().accent);
    assert!(!lines.iter().any(|line| line.contains("Esc cancels this")));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert!(matches!(dashboard.mode, Mode::Confirm(_)));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Right)),
        DashboardAction::None
    );
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::CancelImport
    );
}

#[test]
fn import_safety_defaults_to_ignoring_untracked_files_and_can_include_them() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.show_import_bundle_confirmation(
        vec!["/work/repo — 1 tracked change · 222561 untracked paths".into()],
        Vec::new(),
        Vec::new(),
        true,
        Default::default(),
    );
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw safety warning");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("[✓] Ignore untracked files"));
    assert!(rendered.contains(" Cancel "));
    assert!(rendered.contains(" Continue "));
    assert!(rendered.contains("Space toggles the checkbox."));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::ConfirmImportBundle {
            create_managed_worktree: Some(false),
            accepted: true,
            include_untracked: false,
        }
    );

    dashboard.show_import_bundle_confirmation(
        vec!["/work/repo — 222561 untracked paths".into()],
        Vec::new(),
        Vec::new(),
        true,
        Default::default(),
    );
    dashboard.handle_key(key(KeyCode::Tab));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Char(' '))),
        DashboardAction::None
    );
    dashboard.handle_key(key(KeyCode::BackTab));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::ConfirmImportBundle {
            create_managed_worktree: Some(false),
            accepted: true,
            include_untracked: true,
        }
    );
}

#[test]
fn import_safety_lists_scratch_repositories_left_out_of_the_workspace() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.show_import_bundle_confirmation(
        Vec::new(),
        Vec::new(),
        vec!["/tmp/claude-1000/scratch".into()],
        false,
        Default::default(),
    );
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw safety warning");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(rendered.contains("temporary directories"), "{rendered}");
    assert!(rendered.contains("/tmp/claude-1000/scratch"), "{rendered}");
}

#[test]
fn import_safety_buttons_toggle_the_checkbox_and_cancel_from_the_cancel_button() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.show_import_bundle_confirmation(
        vec!["/work/repo — 1 tracked change · 3 untracked paths".into()],
        Vec::new(),
        Vec::new(),
        true,
        Default::default(),
    );

    // Focus starts on Continue; the checkbox is the next control in the
    // shared form, and moving on to Cancel does not disturb its state.
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Tab)),
        DashboardAction::None
    );
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Char(' '))),
        DashboardAction::None
    );
    let Mode::ConfirmImportBundle(confirmation) = &dashboard.mode else {
        panic!("expected import safety confirmation");
    };
    assert!(!confirmation.ignore_untracked);
    assert_eq!(
        confirmation.form.borrow().focused(),
        Some(DialogControl::ImportIgnore)
    );
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Tab)),
        DashboardAction::None
    );
    let Mode::ConfirmImportBundle(confirmation) = &dashboard.mode else {
        panic!("expected import safety confirmation");
    };
    assert_eq!(
        confirmation.form.borrow().focused(),
        Some(DialogControl::ImportCancel)
    );
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::ConfirmImportBundle {
            create_managed_worktree: None,
            accepted: false,
            include_untracked: false,
        }
    );

    dashboard.show_import_bundle_confirmation(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        false,
        Default::default(),
    );
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Char('y'))),
        DashboardAction::None
    );
    assert!(matches!(dashboard.mode, Mode::ConfirmImportBundle(_)));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Esc)),
        DashboardAction::ConfirmImportBundle {
            create_managed_worktree: None,
            accepted: false,
            include_untracked: false,
        }
    );
}

#[test]
fn importing_session_renders_unknown_then_known_progress_and_ignores_navigation() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.show_import_progress("Chosen session".into());
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Down)),
        DashboardAction::None
    );
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw import progress");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("Importing session · progress 1/?"));

    dashboard.update_import_progress(2, Some(4), "Native session parsed.".into());
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw known import progress");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("Importing session · progress 2/4"));
    assert!(rendered.contains("Native session parsed."));

    let Mode::Importing(progress) = &mut dashboard.mode else {
        panic!("expected import progress");
    };
    progress.last_updated = Instant::now() - IMPORT_STALL_WARNING_AFTER;
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw stalled import progress");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("filesystem may be stalled"));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Esc)),
        DashboardAction::None
    );
    assert!(matches!(dashboard.mode, Mode::Confirm(_)));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Right)),
        DashboardAction::None
    );
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::CancelImport
    );
}

#[test]
fn stop_and_restart_run_from_the_palette_without_a_modal() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let mut dashboard = dashboard_with_session(session);
    dashboard.focus_sessions();
    // Neither command binds a dashboard key any more.
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Char('s'))),
        DashboardAction::None
    );
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Char('r'))),
        DashboardAction::None
    );
    assert_eq!(
        dashboard.dispatch_command(crate::actions::CommandId::StopSession),
        DashboardAction::Close {
            session_id: "session-1".into()
        }
    );
    assert!(matches!(dashboard.mode, Mode::Dashboard));
    assert_eq!(
        dashboard.dispatch_command(crate::actions::CommandId::RestartSession),
        DashboardAction::RestartSession {
            session_id: "session-1".into()
        }
    );
    assert!(matches!(dashboard.mode, Mode::Dashboard));
}

#[test]
fn deleting_a_session_only_asks_yes_or_no() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.focus_sessions();
    dashboard.dispatch_command(crate::actions::CommandId::ForceDestroySession);
    let Mode::Confirm(dialog) = &dashboard.mode else {
        panic!("delete confirmation");
    };
    assert_eq!(confirmation_buttons(&dialog.confirmation), &["No", "Yes"]);
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Char('n'))),
        DashboardAction::None
    );
    assert!(matches!(dashboard.mode, Mode::Dashboard));
    dashboard.dispatch_command(crate::actions::CommandId::ForceDestroySession);
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Char('y'))),
        DashboardAction::ForceDestroy {
            session_id: "session-1".into()
        }
    );
    assert!(matches!(dashboard.mode, Mode::Dashboard));
}

#[test]
fn failed_stop_offers_retry_and_direct_force_stop() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.show_close_failure("session-1".into(), "archive unavailable");
    dashboard.handle_key(key(KeyCode::Right));
    dashboard.handle_key(key(KeyCode::Right));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::Close {
            session_id: "session-1".into()
        }
    );
    dashboard.show_close_failure("session-1".into(), "archive unavailable");
    dashboard.handle_key(key(KeyCode::Right));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::ForceStop {
            session_id: "session-1".into()
        }
    );
    assert!(matches!(dashboard.mode, Mode::Dashboard));
}

#[test]
fn button_confirmations_keep_their_button_row_visible() {
    let confirmations = [
        Confirmation::DestroyStopped {
            session_id: "session-1".into(),
            reopen: None,
        },
        Confirmation::CloseFailed {
            session_id: "session-1".into(),
            error: "archive unavailable".into(),
        },
        Confirmation::ForceDestroy {
            session_id: "session-1".into(),
        },
    ];
    for confirmation in confirmations {
        for (width, height) in [(120, 30), (100, 24), (80, 22)] {
            let mut dashboard = dashboard_with_session(stopped_session());
            dashboard.mode = Mode::Confirm(ConfirmDialog::new(confirmation.clone()));
            let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
            terminal
                .draw(|frame| render(frame, &mut dashboard))
                .expect("draw confirmation");
            let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
            for label in confirmation_buttons(&confirmation) {
                assert!(
                    rendered.contains(&format!(" {label} ")),
                    "{confirmation:?} at {width}x{height} hides {label}"
                );
            }
        }
    }
}

#[test]
fn destroy_stopped_confirmation_destroys_from_its_primary_button() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.show_resume_dialog(1, Vec::new());
    dashboard.handle_key(key(KeyCode::Delete));
    let Mode::Confirm(dialog) = &dashboard.mode else {
        panic!("expected destroy confirmation");
    };
    assert_eq!(confirmation_buttons(&dialog.confirmation), &["No", "Yes"]);
    assert_eq!(
        dialog.form.borrow().focused(),
        Some(DialogControl::ConfirmButton(0))
    );
    dashboard.handle_key(key(KeyCode::Right));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::DestroyStopped {
            session_id: "session-1".into()
        }
    );
    // Destroying from the dialog leaves the user in the dialog.
    assert!(matches!(dashboard.mode, Mode::ResumeDialog(_)));
}

#[test]
fn missing_checkpoint_history_dialog_makes_the_source_field_visible() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.show_repository_origin_dialog(
        "session-1".into(),
        "bifrost".into(),
        "b41dc78".into(),
        "https://github.com/BrokkAi/bifrost.git".into(),
        "BrokkAi/bifrost-dev".into(),
        DashboardAction::None,
    );
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw repository origin dialog");

    let cursor_position = terminal.get_cursor_position().expect("source cursor");
    let buffer = terminal.backend().buffer();
    let lines = buffer_lines(buffer);
    let source_row = lines
        .iter()
        .position(|line| line.contains("Source:"))
        .expect("focused source field");
    let source_y = buffer.area.y + source_row as u16;
    let source_x = buffer.area.x + cell_column(&lines[source_row], "Source:");
    let field_x = source_x + 8;
    assert!(
        buffer[(field_x, source_y)]
            .modifier
            .contains(Modifier::UNDERLINED)
    );
    assert_eq!(
        cursor_position,
        Position {
            x: field_x,
            y: source_y,
        }
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("Type or paste into Source"))
    );

    let button_row = lines
        .iter()
        .position(|line| line.contains(" Cancel ") && line.contains(" Check origin "))
        .expect("button row");
    let button_y = buffer.area.y + button_row as u16;
    let cancel_x = buffer.area.x + cell_column(&lines[button_row], "Cancel");
    let check_x = buffer.area.x + cell_column(&lines[button_row], "Check origin");
    assert_eq!(
        buffer[(cancel_x, button_y)].bg,
        theme::palette().surface_raised
    );
    assert_eq!(
        buffer[(check_x, button_y)].bg,
        theme::palette().surface_raised
    );

    dashboard.handle_key(key(KeyCode::Tab));
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw repository origin dialog with cancel focused");
    let buffer = terminal.backend().buffer();
    let lines = buffer_lines(buffer);
    let button_row = lines
        .iter()
        .position(|line| line.contains(" Cancel ") && line.contains(" Check origin "))
        .expect("button row");
    let button_y = buffer.area.y + button_row as u16;
    let cancel_x = buffer.area.x + cell_column(&lines[button_row], "Cancel");
    assert_eq!(buffer[(cancel_x, button_y)].bg, theme::palette().accent);
    assert!(
        !buffer[(field_x, source_y)]
            .modifier
            .contains(Modifier::UNDERLINED)
    );
}

#[test]
fn missing_checkpoint_history_dialog_accepts_a_replacement_origin() {
    let launch = DashboardAction::ResumeSession {
        workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.into(),
        session_id: "session-1".into(),
        profile_id: "codex-1".into(),
        target_template_id: "podman".into(),
        additional_mounts: Vec::new(),
        resource_allocation: None,
        discard_queue: false,
    };
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.show_repository_origin_dialog(
        "session-1".into(),
        "bifrost".into(),
        "b41dc78".into(),
        "https://github.com/BrokkAi/bifrost.git".into(),
        "BrokkAi/bifrost".into(),
        launch.clone(),
    );
    dashboard.handle_paste("BrokkAi/bifrost-dev\n");

    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::ReplaceResumeRepositoryOrigin {
            session_id: "session-1".into(),
            repository_id: "bifrost".into(),
            replacement: "BrokkAi/bifrost-dev".into(),
            launch: Box::new(launch),
        }
    );
    let Mode::RepositoryOrigin(dialog) = &dashboard.mode else {
        panic!("expected repository origin dialog");
    };
    assert_eq!(dialog.missing_commit, "b41dc78");

    dashboard.apply_repository_origin_failure(
        "bifrost",
        "That origin does not contain checkpoint base b41dc78.".into(),
    );
    let Mode::RepositoryOrigin(dialog) = &dashboard.mode else {
        panic!("expected repository origin dialog");
    };
    assert_eq!(dialog.form.borrow().focused(), Some(DialogControl::Field));
    assert_eq!(
        dialog.error.as_deref(),
        Some("That origin does not contain checkpoint base b41dc78.")
    );
}
#[test]
fn import_confirmation_allows_worktree_opt_out_and_cancellation() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.show_import_bundle_confirmation(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        false,
        mj_core::state::ManagedWorktreeOptions {
            available: true,
            default_create: true,
        },
    );
    let Mode::ConfirmImportBundle(dialog) = &mut dashboard.mode else {
        panic!("import confirmation")
    };
    dialog
        .form
        .get_mut()
        .focus(DialogControl::ImportManagedWorktree);
    dashboard.handle_key(key(KeyCode::Char(' ')));
    let Mode::ConfirmImportBundle(dialog) = &mut dashboard.mode else {
        panic!("import confirmation")
    };
    assert!(!dialog.create_managed_worktree);
    dialog.form.get_mut().focus(DialogControl::ImportContinue);
    assert!(matches!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::ConfirmImportBundle {
            accepted: true,
            create_managed_worktree: Some(false),
            ..
        }
    ));
    dashboard.show_import_bundle_confirmation(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        false,
        mj_core::state::ManagedWorktreeOptions {
            available: true,
            default_create: false,
        },
    );
    let Mode::ConfirmImportBundle(dialog) = &mut dashboard.mode else {
        panic!("import confirmation")
    };
    dialog
        .form
        .get_mut()
        .focus(DialogControl::ImportManagedWorktree);
    dashboard.handle_key(key(KeyCode::Char(' ')));
    assert!(matches!(
        dashboard.handle_key(key(KeyCode::Esc)),
        DashboardAction::ConfirmImportBundle {
            accepted: false,
            create_managed_worktree: None,
            ..
        }
    ));
}
