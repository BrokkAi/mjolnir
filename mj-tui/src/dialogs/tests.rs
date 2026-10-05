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
        subagents: None,
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

/// After a failed launch, the footer still said "Launching demo via codex…"
/// 40 seconds later, under the Launch failed dialog (launch finding R13-8).
/// A success replaces that notice with "Session … is ready"; a failure now
/// replaces it too.
// Hard-won: 968e8ad2: After failure, the footer no longer says Launching and instead shows the terminal failure notice.
#[test]
fn launch_failure_replaces_the_launching_notice() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.set_notice("Launching demo via codex…");

    dashboard.show_launch_failure("Codex is not installed on local host", None);

    assert_eq!(
        dashboard.notice().as_deref(),
        Some("The session could not start.")
    );
    assert!(matches!(dashboard.mode, Mode::Confirm(_)));
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

#[test]
fn the_container_editor_names_the_build_cache_the_session_was_given() {
    let mut session = running_session();
    session.build_cache = Some(mj_core::state::SessionBuildCache {
        host: "ssh:morannon".into(),
        directory: PathBuf::from("/mnt/nvme/mbx"),
        max_size: Some("1000GB".into()),
        target_root: None,
    });
    let mut dashboard = dashboard_with_session(session);
    open_container_editor(&mut dashboard);
    let shown = drawn(&mut dashboard, 120, 40).join("\n");
    assert!(
        shown.contains("Build cache") && shown.contains("/mnt/nvme/mbx"),
        "the session names the cache its container mounts:\n{shown}"
    );
}

#[test]
fn the_container_editor_says_when_a_session_has_no_build_cache() {
    let mut dashboard = dashboard_with_session(running_session());
    open_container_editor(&mut dashboard);
    let shown = drawn(&mut dashboard, 120, 40).join("\n");
    assert!(
        shown.contains("none for this session"),
        "a session without a cache says so:\n{shown}"
    );
}

/// Launch finding R3-11 (with J-24): the container editor named the session
/// by its 32-hex id where the row and every other dialog use its title, and
/// drew its access choice with two dropdown glyphs ("ro · read-only ▾ ▾").
// Hard-won: b313aacd: The header uses the session title instead of its id and the mount access row contains one dropdown glyph.
#[test]
fn the_container_editor_names_the_session_and_draws_one_dropdown_glyph() {
    let mut dashboard = dashboard_with_session(running_session());
    open_container_editor(&mut dashboard);
    let shown = drawn(&mut dashboard, 120, 40).join("\n");
    assert!(shown.contains("Session: ACP pretty name"), "{shown}");
    assert!(!shown.contains("Session: session-1"), "{shown}");
    let access = shown
        .lines()
        .find(|line| line.contains("Access:"))
        .unwrap_or_else(|| panic!("no access row:\n{shown}"));
    assert_eq!(
        access
            .matches(mj_chat::components::ComboBox::glyph())
            .count(),
        1,
        "{access}"
    );
}

fn dashboard_with_container_session() -> DashboardState {
    let mut session = running_session();
    session.additional_mounts = vec![AdditionalMount {
        source: PathBuf::from("/srv/data"),
        destination: PathBuf::from("/mnt/data"),
        access: MountAccess::Cow,
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

/// Reaches a session command the way the user does now: the palette chord, type
/// enough of the name to pick it out, Enter. The session edit dialog
/// these fixtures used to press `e` for no longer exists.
fn through_the_palette(dashboard: &mut DashboardState, query: &str) {
    open_palette(dashboard);
    assert!(
        matches!(dashboard.mode, Mode::Palette(_)),
        "the palette chord opens the palette"
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

#[test]
fn container_editor_edits_a_listed_mount_in_place() {
    let mut dashboard = dashboard_with_container_session();
    open_container_editor(&mut dashboard);

    // A new directory starts read-only; the combobox moves it to
    // copy-on-write.
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
        ContainerEditFocus::Access
    );
    assert_eq!(container_editor(&dashboard).access, MountAccess::Ro);
    dashboard.handle_key(key(KeyCode::Enter));
    assert!(
        container_editor(&dashboard)
            .access_combo
            .is_open(ContainerEditFocus::Access)
    );
    dashboard.handle_key(key(KeyCode::Down));
    dashboard.handle_key(key(KeyCode::Enter));
    assert_eq!(container_editor(&dashboard).access, MountAccess::Cow);
    while container_editor(&dashboard).focused() != ContainerEditFocus::Source {
        dashboard.handle_key(key(KeyCode::Tab));
    }
    dashboard.handle_key(key(KeyCode::Enter));
    assert_eq!(container_editor(&dashboard).mounts.len(), 2);

    // Space on a listed row loads that attachment into the editor fields,
    // where the combobox is the only way to change its access mode.
    while container_editor(&dashboard).focused() != ContainerEditFocus::Mounts {
        dashboard.handle_key(key(KeyCode::Tab));
    }
    dashboard.handle_key(key(KeyCode::Up));
    assert_eq!(container_editor(&dashboard).mount_index, 0);
    dashboard.handle_key(key(KeyCode::Char(' ')));
    assert_eq!(
        container_editor(&dashboard).focused(),
        ContainerEditFocus::Source
    );
    assert_eq!(container_editor(&dashboard).source, "/srv/data");
    assert_eq!(container_editor(&dashboard).destination, "/mnt/data");
    assert_eq!(container_editor(&dashboard).access, MountAccess::Cow);
    assert_eq!(container_editor(&dashboard).editing_mount, Some(0));

    while container_editor(&dashboard).focused() != ContainerEditFocus::Access {
        dashboard.handle_key(key(KeyCode::Tab));
    }
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Down));
    dashboard.handle_key(key(KeyCode::Enter));
    assert_eq!(container_editor(&dashboard).access, MountAccess::Rw);

    // Accepting the edited entry replaces the row it came from instead of
    // attaching the same directory twice.
    while container_editor(&dashboard).focused() != ContainerEditFocus::Source {
        dashboard.handle_key(key(KeyCode::Tab));
    }
    dashboard.handle_key(key(KeyCode::Enter));
    assert_eq!(container_editor(&dashboard).editing_mount, None);
    assert!(container_editor(&dashboard).source.trim().is_empty());

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
                    access: MountAccess::Rw,
                },
                AdditionalMount {
                    source: PathBuf::from("/nfs/share"),
                    destination: PathBuf::from("/mnt/share"),
                    access: MountAccess::Cow,
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
    assert!(rendered.contains("☑ Ignore untracked files"));
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
fn deleting_a_session_keeps_its_branch_unless_asked() {
    let mut dashboard = dashboard_with_session(legacy_managed_session(running_session()));
    dashboard.focus_sessions();
    dashboard.dispatch_command(crate::actions::CommandId::DestroySession);
    let Mode::Confirm(dialog) = &dashboard.mode else {
        panic!("delete confirmation");
    };
    assert_eq!(
        confirmation_buttons(&dialog.confirmation),
        &["Cancel", "Destroy session", "Destroy and delete branch"]
    );
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Char('c'))),
        DashboardAction::None
    );
    assert!(matches!(dashboard.mode, Mode::Dashboard));
    dashboard.dispatch_command(crate::actions::CommandId::DestroySession);
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Char('d'))),
        DashboardAction::ForceDestroy {
            session_id: "session-1".into(),
            delete_branch: false,
        }
    );
    assert!(matches!(dashboard.mode, Mode::Dashboard));
}

#[test]
fn deleting_a_session_takes_its_branch_from_the_last_button() {
    let mut dashboard = dashboard_with_session(legacy_managed_session(running_session()));
    dashboard.focus_sessions();
    dashboard.dispatch_command(crate::actions::CommandId::DestroySession);
    dashboard.handle_key(key(KeyCode::Right));
    dashboard.handle_key(key(KeyCode::Right));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::ForceDestroy {
            session_id: "session-1".into(),
            delete_branch: true,
        }
    );
}

#[test]
fn failed_suspension_requires_a_separate_checkpoint_discard_confirmation() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let checkpoint = session.checkpoint.clone().expect("recovery fixture");
    let mut dashboard = dashboard_with_session(session);
    dashboard.show_close_failure("session-1".into(), "archive unavailable");
    dashboard.handle_key(key(KeyCode::Right));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert!(
        matches!(&dashboard.mode, Mode::Confirm(dialog) if matches!(&dialog.confirmation, Confirmation::DiscardSinceCheckpoint { checkpoint: selected, .. } if selected == &checkpoint))
    );
    dashboard.handle_key(key(KeyCode::Right));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::DiscardSinceCheckpoint {
            session_id: "session-1".into(),
            checkpoint
        }
    );
}

#[test]
fn failed_suspension_without_a_checkpoint_offers_only_retry() {
    let mut session = stopped_session();
    session.checkpoint = None;
    let mut dashboard = dashboard_with_session(session);
    dashboard.show_close_failure("session-1".into(), "archive unavailable");
    let Mode::Confirm(dialog) = &dashboard.mode else {
        panic!("confirmation");
    };
    assert_eq!(
        confirmation_buttons(&dialog.confirmation),
        &["Cancel", "Retry suspension"]
    );
    dashboard.handle_key(key(KeyCode::Right));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::Suspend {
            session_id: "session-1".into(),
            acknowledge_unpublished_work: true,
        }
    );
}

#[test]
fn button_confirmations_keep_their_button_row_visible() {
    let confirmations = [
        Confirmation::DestroyStopped {
            session_id: "session-1".into(),
            delete_branch_available: false,
            reopen: None,
        },
        Confirmation::CloseFailed {
            session_id: "session-1".into(),
            error: "archive unavailable".into(),
            can_discard: true,
        },
        Confirmation::ForceDestroy {
            session_id: "session-1".into(),
            delete_branch_available: false,
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
    open_resume_dialog(&mut dashboard, 1, Vec::new());
    focus_resume_hel_rows(&mut dashboard);
    dashboard.handle_key(key(KeyCode::Delete));
    let Mode::Confirm(dialog) = &dashboard.mode else {
        panic!("expected destroy confirmation");
    };
    assert_eq!(
        confirmation_buttons(&dialog.confirmation),
        &["Cancel", "Destroy session"]
    );
    assert_eq!(
        dialog.form.borrow().focused(),
        Some(DialogControl::ConfirmButton(0))
    );
    dashboard.handle_key(key(KeyCode::Right));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::DestroyStopped {
            session_id: "session-1".into(),
            delete_branch: false,
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
    assert_eq!(Some(buffer[(field_x, source_y)].bg), theme::field(true).bg);
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
    assert_eq!(buffer[(cancel_x, button_y)].bg, theme::palette().selection);
    assert_eq!(buffer[(check_x, button_y)].bg, theme::palette().selection);

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
    assert_eq!(Some(buffer[(field_x, source_y)].bg), theme::field(false).bg);
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

/// The replacement origin may be a URL or `owner/repo`, so completion is
/// offered only while the text reads as a path on this machine.
#[test]
fn repository_origin_completes_local_paths() {
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
        launch,
    );
    let complete = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL);

    dashboard.handle_paste("BrokkAi/bifrost-dev");
    assert_eq!(dashboard.handle_key(complete), DashboardAction::None);

    let Mode::RepositoryOrigin(dialog) = &mut dashboard.mode else {
        panic!("expected repository origin dialog");
    };
    dialog.replacement.set_value("/srv/b");
    assert_eq!(
        dashboard.handle_key(complete),
        DashboardAction::CompletePath {
            host: mj_core::path_completion::CompletionHost::Local,
            kind: mj_core::path_completion::CompletionKind::Directories,
            prefix: "/srv/b".into(),
        }
    );
    let context = dashboard.path_input_context();
    dashboard.apply_path_completions(
        &context,
        "/srv/b",
        mj_core::path_completion::PathCompletion {
            candidates: vec!["/srv/bifrost/".into(), "/srv/bridge/".into()],
            insert: None,
            truncated: false,
        },
    );
    let Mode::RepositoryOrigin(dialog) = &dashboard.mode else {
        panic!("expected repository origin dialog");
    };
    assert!(dialog.replacement.is_completing());

    dashboard.handle_key(key(KeyCode::Enter));
    let Mode::RepositoryOrigin(dialog) = &dashboard.mode else {
        panic!("expected repository origin dialog");
    };
    assert_eq!(dialog.replacement, "/srv/bifrost/");
}
