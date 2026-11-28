use crossterm::event::KeyEvent;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

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
        oracle: None,
        oracle_choice: None,
        remember_container_size: false,
        subagents: None,
        review: None,
        workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.into(),
        profile_id: "codex".into(),
        provider: None,
        target_template_id: "docker".into(),
        bundle_id: "project".into(),
        project_directory: None,
        create_managed_worktree: Some(false),
        additional_mounts: Vec::new(),
        resource_allocation: None,
        carried: Default::default(),
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

/// A failed import used to report only on the footer, which cut the reason
/// off and cleared on the next key press, so pressing Okay seemed to do
/// nothing. The reason now stays in a dialog until dismissed.
#[test]
fn import_failure_shows_the_reason_until_dismissed() {
    let mut dashboard = dashboard_with_session(stopped_session());
    let previous = dashboard.mode.clone();
    dashboard.show_import_failure("the project's Git repository has no remote");
    assert_eq!(
        dashboard.notice().as_deref(),
        Some("The session import failed.")
    );
    let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
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
    assert!(text.contains("Import failed"), "{text}");
    assert!(text.contains("has no remote"), "{text}");
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert_eq!(dashboard.mode, previous);
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
            resource_allocation: None,
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
fn import_safety_keeps_managed_checkout_choice_without_file_inclusion_toggle() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.show_import_bundle_confirmation(
        vec!["/work/repo — 1 tracked change · 222561 untracked paths".into()],
        Vec::new(),
        Vec::new(),
        mj_core::state::ManagedWorktreeOptions {
            available: true,
            default_create: false,
        },
        "/work/repo".into(),
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
    assert!(rendered.contains("☐ Create isolated checkout"));
    assert!(rendered.contains("Checked creates a clean, separate checkout."));
    assert!(rendered.contains("These checkouts have uncommitted changes:"));
    assert!(rendered.contains(" Cancel "));
    assert!(rendered.contains(" Continue "));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::ConfirmImportBundle {
            project_root: "/work/repo".into(),
            create_managed_worktree: Some(false),
            accepted: true,
        }
    );

    dashboard.show_import_bundle_confirmation(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        mj_core::state::ManagedWorktreeOptions {
            available: true,
            default_create: false,
        },
        "/work/repo".into(),
    );
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw managed checkout choice");
    for _ in 0..3 {
        if matches!(dashboard.mode, Mode::ConfirmImportBundle(ref dialog)
            if dialog.form.borrow().focused() == Some(DialogControl::ImportManagedWorktree))
        {
            break;
        }
        dashboard.handle_key(key(KeyCode::Tab));
    }
    assert!(
        matches!(dashboard.mode, Mode::ConfirmImportBundle(ref dialog)
        if dialog.form.borrow().focused() == Some(DialogControl::ImportManagedWorktree))
    );
    dashboard.handle_key(key(KeyCode::Char(' ')));
    for _ in 0..3 {
        if matches!(dashboard.mode, Mode::ConfirmImportBundle(ref dialog)
            if dialog.form.borrow().focused() == Some(DialogControl::ImportContinue))
        {
            break;
        }
        dashboard.handle_key(key(KeyCode::Tab));
    }
    assert!(
        matches!(dashboard.mode, Mode::ConfirmImportBundle(ref dialog)
        if dialog.form.borrow().focused() == Some(DialogControl::ImportContinue))
    );
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::ConfirmImportBundle {
            project_root: "/work/repo".into(),
            create_managed_worktree: Some(true),
            accepted: true,
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
fn deleting_a_session_keeps_its_branch_unless_asked() {
    let mut dashboard = dashboard_with_session(legacy_managed_session(running_session()));
    dashboard.focus_sessions();
    dashboard.dispatch_command(crate::actions::CommandId::DestroySession);
    let Mode::Confirm(dialog) = &dashboard.mode else {
        panic!("delete confirmation");
    };
    assert_eq!(
        confirmation_buttons(&dialog.confirmation),
        vec!["Cancel", "Destroy session"]
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
        }
    );
    assert!(matches!(dashboard.mode, Mode::Dashboard));
}

#[test]
fn failed_suspension_offers_retry_and_suspend_without_checkpoint() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let mut dashboard = dashboard_with_session(session);
    dashboard.show_close_failure("session-1".into(), "archive unavailable");
    let Mode::Confirm(dialog) = &dashboard.mode else {
        panic!("confirmation");
    };
    assert_eq!(
        confirmation_buttons(&dialog.confirmation),
        vec!["Cancel", "Retry suspension", "Suspend without checkpoint…"]
    );
    dashboard.handle_key(key(KeyCode::Right));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::Suspend {
            session_id: "session-1".into(),
            consent: mj_core::suspend_consent::SuspendConsent::default(),
        }
    );
    dashboard.show_close_failure("session-1".into(), "archive unavailable");
    dashboard.handle_key(key(KeyCode::Right));
    dashboard.handle_key(key(KeyCode::Right));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::Suspend {
            session_id: "session-1".into(),
            consent: mj_core::suspend_consent::SuspendConsent {
                without_checkpoint: true,
                confirm_no_checkpoint: true,
                history_only: false,
                ..Default::default()
            },
        }
    );
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
    open_resume_dialog(&mut dashboard, 1, Vec::new());
    focus_resume_hel_rows(&mut dashboard);
    dashboard.handle_key(key(KeyCode::Delete));
    let Mode::Confirm(dialog) = &dashboard.mode else {
        panic!("expected destroy confirmation");
    };
    assert_eq!(
        confirmation_buttons(&dialog.confirmation),
        vec!["Cancel", "Destroy session"]
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
        }
    );
    // Destroying from the dialog leaves the user in the dialog.
    assert!(matches!(dashboard.mode, Mode::ResumeDialog(_)));
}

fn append_dialog_render(
    output: &mut String,
    label: &str,
    width: u16,
    height: u16,
    lines: &[String],
) {
    use std::fmt::Write as _;
    if !output.is_empty() {
        output.push('\n');
    }
    writeln!(output, "=== {label} ({width}x{height}) ===").unwrap();
    output.push_str(&lines.join("\n"));
    output.push('\n');
}

fn click_dialog_label(
    dashboard: &mut DashboardState,
    lines: &[String],
    label: &str,
) -> DashboardAction {
    let position = point(lines, label);
    let mut action = DashboardAction::None;
    for kind in [
        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
    ] {
        action = dashboard.handle_mouse(mouse_at(kind, position));
    }
    action
}

fn normalize_notice_log_ages(lines: &mut [String]) {
    // The renderer's relative age is intentionally replaced with a same-width
    // marker so the golden pins the complete modal without pinning wall time.
    for line in lines {
        if let Some(ago) = line.find(" ago ") {
            let token_start = line[..ago].rfind(' ').map_or(0, |space| space + 1);
            line.replace_range(token_start..ago, &"?".repeat(ago - token_start));
        }
    }
}

#[test]
fn golden_dashboard_dialogs() {
    use std::fmt::Write as _;

    let mut output = String::new();

    {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.show_close_failure("session-1".into(), "archive unavailable");
        let lines = drawn(&mut dashboard, 100, 28);
        append_dialog_render(&mut output, "close failure", 100, 28, &lines);
        let action = dashboard.handle_key(key(KeyCode::Char('r')));
        assert_eq!(
            action,
            DashboardAction::Suspend {
                session_id: "session-1".into(),
                consent: mj_core::suspend_consent::SuspendConsent::default(),
            }
        );
        writeln!(output, "action: {action:?}").unwrap();
    }

    // Suspend dialog with untracked files, in-progress operation, and raw checkout.
    for (label, preflight) in [
        (
            "suspend dialog with untracked files",
            mj_core::suspend_consent::SuspendPreflight {
                session_id: "session-1".into(),
                untracked: (1..=7)
                    .map(|i| mj_core::suspend_consent::UntrackedPath {
                        checkout: mj_core::suspend_consent::CheckoutKey::default(),
                        path: std::path::PathBuf::from(format!("scratch-{i}.txt")),
                        bytes: 128,
                        identity: mj_core::suspend_consent::FileIdentity::new(format!("id-{i}")),
                    })
                    .collect(),
                ..Default::default()
            },
        ),
        (
            "suspend dialog with in-progress operation",
            mj_core::suspend_consent::SuspendPreflight {
                session_id: "session-1".into(),
                operations: vec![mj_core::suspend_consent::ShownOperation {
                    checkout: mj_core::suspend_consent::CheckoutKey::default(),
                    kind: mj_core::suspend_consent::GitOperationKind::Rebase,
                }],
                ..Default::default()
            },
        ),
        (
            "suspend dialog with raw checkout",
            mj_core::suspend_consent::SuspendPreflight {
                session_id: "session-1".into(),
                raw: true,
                ..Default::default()
            },
        ),
        (
            "suspend dialog with blockers",
            mj_core::suspend_consent::SuspendPreflight {
                session_id: "session-1".into(),
                blockers: vec![mj_core::suspend_consent::SuspendBlocker {
                    kind: "hidden_tracked_edits".into(),
                    paths: (1..=7).map(|i| format!("linked/file-{i}.txt")).collect(),
                    remedy: "Clear index flags in the linked checkout before suspending.".into(),
                }],
                ..Default::default()
            },
        ),
        // Hard-won: #1679: backend-only warnings showed Continue anyway without a reason.
        (
            "suspend dialog with backend-only warnings",
            mj_core::suspend_consent::SuspendPreflight {
                session_id: "session-1".into(),
                children_not_handed_back: vec!["native-child".into()],
                interrupting: true,
                ..Default::default()
            },
        ),
    ] {
        let mut dashboard = dashboard_with_session(running_session());
        let request_id = 1;
        dashboard.mode = Mode::Confirm(ConfirmDialog::new(Confirmation::SuspendSession {
            session_id: "session-1".into(),
            request_id,
            child_warning_titles: Vec::new(),
            preflight: None,
            viewing_all_untracked: false,
            resolving_blockers: false,
            confirming_history_only: false,
            can_prompt: true,
        }));
        assert_eq!(
            dashboard.apply_suspend_preflight(request_id, preflight),
            DashboardAction::None
        );
        let lines = drawn(&mut dashboard, 100, 28);
        append_dialog_render(&mut output, label, 100, 28, &lines);
        if label == "suspend dialog with blockers" {
            dashboard.handle_key(key(KeyCode::Char('v')));
            let lines = drawn(&mut dashboard, 100, 28);
            append_dialog_render(&mut output, "suspend blockers expanded", 100, 28, &lines);
            let Mode::Confirm(dialog) = &dashboard.mode else {
                panic!("confirmation")
            };
            let confirmation = dialog.confirmation.clone();
            let history = confirmation_buttons(&confirmation)
                .iter()
                .position(|label| label == "Suspend (history only)…")
                .unwrap();
            dashboard.activate_confirmation_button(confirmation, history);
            append_dialog_render(
                &mut output,
                "history-only external worktree warning",
                100,
                28,
                &drawn(&mut dashboard, 100, 28),
            );
            let Mode::Confirm(dialog) = &dashboard.mode else {
                panic!("confirmation")
            };
            let confirmation = dialog.confirmation.clone();
            dashboard.activate_confirmation_button(confirmation, 0);
            let prompt = dashboard.handle_key(key(KeyCode::Char('a')));
            assert!(
                matches!(prompt, DashboardAction::PromptSession { ref text, .. } if text.contains("linked/file-7.txt") && text.contains("Clear index flags"))
            );
        }
    }

    let mut targets = dashboard_with_session(running_session());
    targets.cycle_focus(false);
    targets.cycle_focus(false);
    let action = targets.handle_key(key(KeyCode::Char('e')));
    writeln!(output, "action: {action:?}").unwrap();
    let lines = drawn(&mut targets, 120, 34);
    append_dialog_render(&mut output, "target actions", 120, 34, &lines);
    let action = click_dialog_label(&mut targets, &lines, "Test");
    writeln!(output, "action: {action:?}").unwrap();
    targets.apply_target_test("podman".into(), Ok(()));
    let lines = drawn(&mut targets, 120, 34);
    append_dialog_render(&mut output, "target test result", 120, 34, &lines);

    let _ = click_dialog_label(&mut targets, &lines, "Rename");
    let lines = drawn(&mut targets, 120, 34);
    append_dialog_render(&mut output, "target ID editor", 120, 34, &lines);
    targets.handle_key(key(KeyCode::End));
    for character in ['-', 'v', '2'] {
        targets.handle_key(key(KeyCode::Char(character)));
    }
    let lines = drawn(&mut targets, 120, 34);
    append_dialog_render(&mut output, "edited target ID", 120, 34, &lines);
    let action = targets.handle_key(key(KeyCode::Enter));
    writeln!(output, "action: {action:?}").unwrap();

    let action = targets.handle_key(key(KeyCode::Char('e')));
    writeln!(output, "action: {action:?}").unwrap();
    let lines = drawn(&mut targets, 120, 34);
    let _ = click_dialog_label(&mut targets, &lines, "Rename");
    let action = targets.handle_key(key(KeyCode::Esc));
    writeln!(output, "action: {action:?}").unwrap();
    append_dialog_render(
        &mut output,
        "cancelled target ID editor returns to actions",
        120,
        34,
        &drawn(&mut targets, 120, 34),
    );

    let mut changed = dashboard_with_session(running_session());
    changed.focus_sessions();
    let action = chord(&mut changed, crate::CommandId::ChangedFiles);
    writeln!(output, "action: {action:?}").unwrap();
    append_dialog_render(
        &mut output,
        "changed files while the checkout probe is pending",
        100,
        24,
        &drawn(&mut changed, 100, 24),
    );
    let mut numstat = String::new();
    let mut porcelain = String::new();
    for index in 0..36 {
        write!(
            numstat,
            "{}\t{}\tsrc/module-{index:02}.rs\0",
            index + 1,
            index
        )
        .unwrap();
        write!(porcelain, " M src/module-{index:02}.rs\0").unwrap();
    }
    changed.set_git_status(
        "session-1".into(),
        Ok(mj_core::local_git::parse_git_status(
            "/workspace/project".into(),
            "feature/dialogs",
            Some("1\t3"),
            &numstat,
            &porcelain,
        )),
    );
    let lines = drawn(&mut changed, 100, 24);
    append_dialog_render(&mut output, "changed files first page", 100, 24, &lines);
    changed.handle_key(key(KeyCode::PageDown));
    append_dialog_render(
        &mut output,
        "changed files after paging down",
        100,
        24,
        &drawn(&mut changed, 100, 24),
    );
    let action = changed.handle_key(key(KeyCode::Char('r')));
    writeln!(output, "action: {action:?}").unwrap();
    let action = changed.handle_key(key(KeyCode::Esc));
    writeln!(output, "action: {action:?}").unwrap();
    append_dialog_render(
        &mut output,
        "dashboard after closing changed files",
        100,
        24,
        &drawn(&mut changed, 100, 24),
    );

    let mut notices = dashboard_with_session(running_session());
    for index in 0..12 {
        notices.set_failure_notice(format!(
            "Build step {index:02} failed: the remote worker could not start its checkout or load the requested profile"
        ));
    }
    open_palette(&mut notices);
    for character in "Recent messages".chars() {
        notices.handle_key(key(KeyCode::Char(character)));
    }
    append_dialog_render(
        &mut output,
        "command palette search for recent messages",
        100,
        28,
        &drawn(&mut notices, 100, 28),
    );
    let action = notices.handle_key(key(KeyCode::Enter));
    writeln!(output, "action: {action:?}").unwrap();
    let mut lines = drawn(&mut notices, 100, 28);
    normalize_notice_log_ages(&mut lines);
    append_dialog_render(&mut output, "recent messages", 100, 28, &lines);
    notices.handle_key(key(KeyCode::PageDown));
    let mut lines = drawn(&mut notices, 100, 28);
    normalize_notice_log_ages(&mut lines);
    append_dialog_render(
        &mut output,
        "recent messages after paging down",
        100,
        28,
        &lines,
    );
    notices.handle_key(key(KeyCode::Home));
    let action = notices.handle_key(key(KeyCode::Esc));
    writeln!(output, "action: {action:?}").unwrap();

    let mut cpu = dashboard_with_session(running_session());
    let mut session_cpu = mj_core::snapshot_map::SnapshotMap::new();
    session_cpu.insert(
        "session-1".into(),
        mj_client::runtime_feed::SessionCpuView::Measured {
            usage: mj_core::cpu_usage::SessionCpuUsage {
                recent_permille: 230,
                hourly_permille: 180,
                hourly_covered_secs: 3600,
                online_cpus: 8,
                scope: mj_core::cpu_usage::CpuSampleScope::ProcessTree,
            },
        },
    );
    for index in 2..=18 {
        let id = format!("session-{index:02}");
        let mut session = running_session();
        session.id = id.clone();
        session.title = format!("CPU session {index:02}");
        session.acp_session_title = None;
        cpu.state.sessions.insert(id.clone(), session);
        session_cpu.insert(
            id,
            mj_client::runtime_feed::SessionCpuView::Measured {
                usage: mj_core::cpu_usage::SessionCpuUsage {
                    recent_permille: 100 + index as u16,
                    hourly_permille: 80 + index as u16,
                    hourly_covered_secs: 3600,
                    online_cpus: 8,
                    scope: mj_core::cpu_usage::CpuSampleScope::ProcessTree,
                },
            },
        );
    }
    crate::test_support::publish_fixture_verdicts(&mut cpu);
    cpu.set_session_cpu(session_cpu);
    open_palette(&mut cpu);
    for character in "CPU by session".chars() {
        cpu.handle_key(key(KeyCode::Char(character)));
    }
    append_dialog_render(
        &mut output,
        "command palette search for CPU by session",
        120,
        36,
        &drawn(&mut cpu, 120, 36),
    );
    let action = cpu.handle_key(key(KeyCode::Enter));
    writeln!(output, "action: {action:?}").unwrap();
    append_dialog_render(
        &mut output,
        "CPU by session",
        120,
        36,
        &drawn(&mut cpu, 120, 36),
    );
    cpu.handle_key(key(KeyCode::PageDown));
    append_dialog_render(
        &mut output,
        "CPU by session after paging down",
        120,
        36,
        &drawn(&mut cpu, 120, 36),
    );
    let action = cpu.handle_key(key(KeyCode::Esc));
    writeln!(output, "action: {action:?}").unwrap();

    // Extend the full dialog golden through the Profiles menu and real controls.
    use mj_core::usage::{
        AgentUsageEstimate, HOUR_MS, HourlyUsage, ModelUsageEstimate, ProfileQuotaUsage,
        ProfileUsageEstimate, SessionUsageEstimate, SessionUsageReport, UnknownCost,
        UsageAgentRole,
    };
    let mut usage = dashboard_with_session(running_session());
    usage.begin_support_pane_menu(crate::SupportPane::Quota);
    append_dialog_render(
        &mut output,
        "Profiles menu with usage report",
        120,
        36,
        &drawn(&mut usage, 120, 36),
    );
    let claude_profile = usage.config.profiles["claude-1"].clone();
    let codex_profile = usage.config.profiles["codex-1"].clone();
    usage
        .config
        .profiles
        .insert("claude".into(), claude_profile.clone());
    usage.config.profiles.insert("codex".into(), codex_profile);
    usage
        .config
        .profiles
        .insert("claude-idle".into(), claude_profile);
    for (id, profile) in &mut usage.config.profiles {
        profile.enabled = matches!(id.as_str(), "claude" | "claude-idle" | "codex");
    }
    usage.quota_providers = crate::render::quota_pools::ProviderDiscovery::start(&usage.config);
    usage.handle_key(key(KeyCode::Char('3')));
    append_dialog_render(
        &mut output,
        "Usage Dashboard loading",
        120,
        36,
        &drawn(&mut usage, 120, 36),
    );
    let (first, query) = usage.session_usage_request().unwrap();
    assert_eq!(query, mj_core::usage::SessionUsageQuery::default());
    let deepseek_flash = ModelUsageEstimate {
        model: Some("deepseek-flash".into()),
        unreported_turns: 2,
        cost_usd_ticks: Ok(0),
        ..Default::default()
    };
    let mut report = SessionUsageReport {
        profiles: vec!["claude".into(), "codex".into()],
        cost_usd_ticks: Err(UnknownCost),
        sessions: vec![
            SessionUsageEstimate {
                session_id: "session-2".into(),
                title: "Write release notes".into(),
                profile: "claude".into(),
                cost_usd_ticks: Ok(82_000_000_000),
                models: vec![
                    ModelUsageEstimate {
                        model: Some("claude-opus-5-5".into()),
                        cost_usd_ticks: Ok(62_000_000_000),
                        ..Default::default()
                    },
                    ModelUsageEstimate {
                        model: Some("gpt-6.1-sol".into()),
                        totals: std::collections::BTreeMap::from([(
                            "total_tokens".into(),
                            mj_core::storage::UsageCounterTotal {
                                tokens: 1_100_000,
                                reported_turns: 1,
                            },
                        )]),
                        unreported_turns: 1,
                        cost_usd_ticks: Ok(20_000_000_000),
                    },
                ],
                coverage: mj_core::storage::UsageCoverage {
                    recorded_turns: 5,
                    full_turn_reports: 3,
                    missing_reports: 2,
                    ..Default::default()
                },
                ..Default::default()
            },
            SessionUsageEstimate {
                session_id: "session-1".into(),
                title: "Investigate recovery".into(),
                profile: "codex".into(),
                subagent_count: 2,
                cost_usd_ticks: Err(UnknownCost),
                models: vec![
                    ModelUsageEstimate {
                        model: Some("gpt-6.1-sol".into()),
                        cost_usd_ticks: Ok(30_000_000_000),
                        ..Default::default()
                    },
                    ModelUsageEstimate {
                        model: Some("gpt-6.1-terra".into()),
                        cost_usd_ticks: Ok(10_000_000_000),
                        ..Default::default()
                    },
                    ModelUsageEstimate {
                        model: Some("claude-opus-5-5".into()),
                        cost_usd_ticks: Ok(71_000_000_000),
                        ..Default::default()
                    },
                    ModelUsageEstimate {
                        model: Some("gpt-6-luna".into()),
                        cost_usd_ticks: Ok(1_050_000_000),
                        ..Default::default()
                    },
                    ModelUsageEstimate {
                        model: Some("custom-model".into()),
                        cost_usd_ticks: Err(UnknownCost),
                        ..Default::default()
                    },
                    deepseek_flash.clone(),
                ],
                coverage: mj_core::storage::UsageCoverage {
                    recorded_turns: 6,
                    full_turn_reports: 4,
                    missing_reports: 2,
                    ..Default::default()
                },
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let role_model = |role, model: ModelUsageEstimate| AgentUsageEstimate {
        role,
        cost_usd_ticks: model.cost_usd_ticks,
        models: vec![model],
    };
    let codex_models = &report.sessions[1].models;
    let split_sol = |cost| ModelUsageEstimate {
        cost_usd_ticks: Ok(cost),
        ..codex_models[0].clone()
    };
    report.agent_usage = vec![
        AgentUsageEstimate {
            role: UsageAgentRole::Primary,
            models: vec![
                report.sessions[0].models[0].clone(),
                codex_models[1].clone(),
            ],
            cost_usd_ticks: Ok(72_000_000_000),
        },
        role_model(UsageAgentRole::Orchestrator, split_sol(20_000_000_000)),
        AgentUsageEstimate {
            role: UsageAgentRole::Subtle,
            models: vec![split_sol(15_000_000_000), deepseek_flash.clone()],
            cost_usd_ticks: Ok(15_000_000_000),
        },
        role_model(UsageAgentRole::Straightforward, split_sol(10_000_000_000)),
        role_model(UsageAgentRole::Oracle, split_sol(5_000_000_000)),
        role_model(UsageAgentRole::Reviewer, codex_models[2].clone()),
        role_model(UsageAgentRole::Subagent, codex_models[3].clone()),
        role_model(
            UsageAgentRole::UnclassifiedSubagent,
            codex_models[4].clone(),
        ),
    ];
    report.profile_usage = vec![
        ProfileUsageEstimate {
            profile: Some("claude".into()),
            cost_usd_ticks: Ok(82_000_000_000),
            coverage: report.sessions[0].coverage.clone(),
        },
        ProfileUsageEstimate {
            profile: Some("codex".into()),
            cost_usd_ticks: Err(UnknownCost),
            ..Default::default()
        },
    ];
    report.quota_usage = vec![
        ProfileQuotaUsage {
            profile: "claude".into(),
            consumed_basis_points: Some(1000),
            covered_seconds: Some(86_400),
            partial: false,
            monthly_price_usd: Some(200),
        },
        ProfileQuotaUsage {
            profile: "claude-idle".into(),
            consumed_basis_points: Some(0),
            covered_seconds: Some(86_400),
            partial: false,
            monthly_price_usd: Some(100),
        },
        ProfileQuotaUsage {
            profile: "codex".into(),
            consumed_basis_points: Some(2000),
            covered_seconds: Some(86_400),
            partial: false,
            monthly_price_usd: Some(200),
        },
    ];
    // Dollars per hour; the 20th bar also ran a model with no known rate, and
    // the 14th ran only such models.
    let hour_dollars = [
        0u64, 0, 3, 8, 5, 0, 0, 12, 30, 22, 18, 9, 4, 0, 0, 0, 6, 14, 26, 41, 35, 20, 11, 7,
    ];
    report.hours = hour_dollars
        .iter()
        .enumerate()
        .map(|(n, dollars)| HourlyUsage {
            start_ms: (490_000 + n as i64) * HOUR_MS,
            known_cost_usd_ticks: dollars * 10_000_000_000,
            has_unknown_cost: n == 19 || n == 13,
        })
        .collect();
    let mut known = report.clone();
    known.profile_usage[1].cost_usd_ticks = Ok(30_000_000_000);
    known.cost_usd_ticks = Ok(112_000_000_000);
    // The idle member still makes the provider's quota coverage incomplete.
    known.quota_usage[1].partial = true;
    usage.apply_session_usage_result(first, Ok(known));
    append_dialog_render(
        &mut output,
        "Usage known provider totals with partial coverage",
        120,
        36,
        &drawn(&mut usage, 120, 36),
    );
    let mut unweighted = report.clone();
    unweighted.quota_usage[1].monthly_price_usd = None;
    usage.apply_session_usage_result(first, Ok(unweighted));
    append_dialog_render(
        &mut output,
        "Usage unknown plan weight",
        70,
        22,
        &drawn(&mut usage, 70, 22),
    );
    let mut missing = report.clone();
    missing.quota_usage[1].consumed_basis_points = None;
    missing.quota_usage[1].partial = true;
    usage.apply_session_usage_result(first, Ok(missing));
    append_dialog_render(
        &mut output,
        "Usage missing idle account reading",
        70,
        17,
        &drawn(&mut usage, 70, 17),
    );
    let mut many_providers = report.clone();
    many_providers
        .profile_usage
        .extend((0..12).map(|n| ProfileUsageEstimate {
            profile: Some(format!("retired-account-with-long-profile-name-{n:02}")),
            cost_usd_ticks: Ok(5_000_000_000),
            ..Default::default()
        }));
    usage.apply_session_usage_result(first, Ok(many_providers));
    append_dialog_render(
        &mut output,
        "Usage many providers preserves details and chart footer",
        70,
        30,
        &drawn(&mut usage, 70, 30),
    );
    let mut retained = report.clone();
    retained.profile_usage.extend([
        ProfileUsageEstimate {
            profile: Some("retired-account".into()),
            cost_usd_ticks: Ok(5_000_000_000),
            ..Default::default()
        },
        ProfileUsageEstimate {
            profile: None,
            cost_usd_ticks: Err(UnknownCost),
            ..Default::default()
        },
    ]);
    usage.quota_providers = Default::default();
    usage.apply_session_usage_result(first, Ok(retained));
    append_dialog_render(
        &mut output,
        "Usage unresolved and retained provider labels",
        120,
        36,
        &drawn(&mut usage, 120, 36),
    );
    append_dialog_render(
        &mut output,
        "Usage unresolved and retained providers",
        70,
        17,
        &drawn(&mut usage, 70, 17),
    );
    usage.quota_providers = crate::render::quota_pools::ProviderDiscovery::start(&usage.config);
    let now = mj_core::clock::epoch_seconds() as i64;
    let mut forecast_report = report.clone();
    forecast_report.quota_projections = vec![mj_core::usage::ProfileQuotaProjection {
        profile_id: "codex4".into(),
        window_label: "Week".into(),
        is_short_window: false,
        remaining_percent: 38,
        resets_at_epoch_seconds: Some(now + 54 * 3600 + 30),
        observed_pct_per_hour: Some(1.0),
        sustainable_pct_per_hour: Some(0.625),
        pace_ratio: Some(1.6),
        projected_exhaustion_epoch_seconds: Some(now + 28 * 3600 + 30),
    }];
    usage.apply_session_usage_result(first, Ok(forecast_report));
    append_dialog_render(
        &mut output,
        "Usage quota forecast",
        120,
        36,
        &drawn(&mut usage, 120, 36),
    );
    usage.apply_session_usage_result(first, Ok(report.clone()));
    append_dialog_render(
        &mut output,
        "Usage Dashboard All with model costs",
        120,
        36,
        &drawn(&mut usage, 120, 36),
    );
    // Hard-won: #1501: a wrapped fixed header clipped models and crowded out sessions.
    if let Mode::SessionUsageReport(dialog) = &mut usage.mode {
        dialog
            .form
            .get_mut()
            .focus(crate::session_usage_report::UsageControl::Rows);
    }
    usage.handle_key(key(KeyCode::Home));
    for _ in 0..16 {
        usage.handle_key(key(KeyCode::Down));
    }
    append_dialog_render(
        &mut output,
        "Usage Dashboard All with sessions",
        120,
        36,
        &drawn(&mut usage, 120, 36),
    );
    append_dialog_render(
        &mut output,
        "Usage narrow session list",
        70,
        22,
        &drawn(&mut usage, 70, 22),
    );
    usage.handle_key(key(KeyCode::End));
    append_dialog_render(
        &mut output,
        "Usage narrow last session",
        70,
        22,
        &drawn(&mut usage, 70, 22),
    );
    usage.handle_key(key(KeyCode::Home));
    if let Mode::SessionUsageReport(dialog) = &mut usage.mode {
        dialog
            .form
            .get_mut()
            .focus(crate::session_usage_report::UsageControl::Profile);
    }
    drawn(&mut usage, 120, 36);
    usage.handle_key(key(KeyCode::Enter));
    append_dialog_render(
        &mut output,
        "Usage profile dropdown",
        120,
        36,
        &drawn(&mut usage, 120, 36),
    );
    let Mode::SessionUsageReport(dialog) = &usage.mode else {
        panic!("usage dialog closed");
    };
    let codex_index = dialog.profiles.iter().position(|p| p == "codex").unwrap() + 1;
    for _ in 0..codex_index {
        usage.handle_key(key(KeyCode::Down));
    }
    usage.handle_key(key(KeyCode::Enter));
    let (profile_request, query) = usage.session_usage_request().unwrap();
    assert_eq!(query.profile.as_deref(), Some("codex"));
    // The old All result cannot replace the filtered loading state.
    usage.apply_session_usage_result(first, Ok(report.clone()));
    assert!(matches!(&usage.mode,Mode::SessionUsageReport(dialog) if dialog.report.is_none()));
    let mut codex_report = report.clone();
    codex_report
        .profile_usage
        .retain(|usage| usage.profile.as_deref() == Some("codex"));
    codex_report
        .quota_usage
        .retain(|quota| quota.profile == "codex");
    codex_report.sessions = vec![report.sessions[1].clone()];
    codex_report.sessions[0]
        .models
        .retain(|model| model.model.as_deref() != Some("claude-opus-5-5"));
    codex_report.agent_usage = vec![
        role_model(UsageAgentRole::Primary, codex_models[1].clone()),
        role_model(UsageAgentRole::Orchestrator, split_sol(10_000_000_000)),
        AgentUsageEstimate {
            role: UsageAgentRole::Subtle,
            models: vec![split_sol(10_000_000_000), deepseek_flash.clone()],
            cost_usd_ticks: Ok(10_000_000_000),
        },
        role_model(UsageAgentRole::Straightforward, split_sol(5_000_000_000)),
        role_model(UsageAgentRole::Oracle, split_sol(5_000_000_000)),
        AgentUsageEstimate {
            role: UsageAgentRole::Reviewer,
            models: Vec::new(),
            cost_usd_ticks: Ok(0),
        },
        role_model(UsageAgentRole::Subagent, codex_models[3].clone()),
        role_model(
            UsageAgentRole::UnclassifiedSubagent,
            codex_models[4].clone(),
        ),
    ];
    usage.apply_session_usage_result(profile_request, Ok(codex_report.clone()));
    // Hard-won: #1501: unknown totals hid the known per-model sums in both views.
    append_dialog_render(
        &mut output,
        "Usage Codex with session models",
        120,
        36,
        &drawn(&mut usage, 120, 36),
    );
    // Clicking the first bar with cost selects its hour, and the rows reload.
    let lines = drawn(&mut usage, 120, 36);
    let base_row = lines.iter().position(|line| line.contains("$0 ")).unwrap();
    let column = lines[base_row].chars().position(|c| c == '▂').unwrap();
    let next_column = lines[base_row].chars().position(|c| c == '▆').unwrap();
    let pitch = next_column - column;
    let click = |usage: &mut DashboardState, position: (u16, u16), modifiers| {
        for kind in [
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
        ] {
            let mut event = mouse_at(kind, position);
            event.modifiers = modifiers;
            usage.handle_mouse(event);
        }
    };
    let at = |index: usize| ((column + (index - 2) * pitch) as u16, base_row as u16);
    let hour = |index: i64| (490_000 + index) * HOUR_MS;
    let selected = |indexes: &[i64]| {
        Some(
            indexes
                .iter()
                .map(|index| hour(*index))
                .collect::<std::collections::BTreeSet<_>>(),
        )
    };
    click(&mut usage, at(2), KeyModifiers::NONE);
    let (hour_request, query) = usage.session_usage_request().unwrap();
    assert_eq!(query.hour_starts_ms, selected(&[2]));
    let mut hour_report = codex_report.clone();
    hour_report.quota_usage.iter_mut().for_each(|q| {
        q.consumed_basis_points = Some(125);
        q.partial = true;
    });
    usage.apply_session_usage_result(hour_request, Ok(hour_report));
    append_dialog_render(
        &mut output,
        "Usage one hour selected",
        120,
        36,
        &drawn(&mut usage, 120, 36),
    );
    // A plain click stays on the hour. Ctrl adds a discontiguous hour.
    click(&mut usage, at(2), KeyModifiers::NONE);
    assert_eq!(
        usage.session_usage_request().unwrap().1.hour_starts_ms,
        selected(&[2])
    );
    click(&mut usage, at(7), KeyModifiers::CONTROL);
    let (multi_request, query) = usage.session_usage_request().unwrap();
    assert_eq!(query.hour_starts_ms, selected(&[2, 7]));
    usage.apply_session_usage_result(multi_request, Ok(codex_report.clone()));
    append_dialog_render(
        &mut output,
        "Usage discontiguous hours selected",
        120,
        36,
        &drawn(&mut usage, 120, 36),
    );
    // The text golden cannot show bar colors, so check both selected bars and the gap.
    let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
    terminal.draw(|frame| render(frame, &mut usage)).unwrap();
    let buffer = terminal.backend().buffer();
    let style_at = |index| buffer[(at(index).0, at(index).1)].style();
    assert_eq!(style_at(2), style_at(7));
    assert_ne!(style_at(2), style_at(3));
    // Shift retains the Ctrl-click anchor while replacing the range.
    click(&mut usage, at(5), KeyModifiers::SHIFT);
    assert_eq!(
        usage.session_usage_request().unwrap().1.hour_starts_ms,
        selected(&[5, 6, 7])
    );
    click(&mut usage, at(3), KeyModifiers::SHIFT);
    assert_eq!(
        usage.session_usage_request().unwrap().1.hour_starts_ms,
        selected(&[3, 4, 5, 6, 7])
    );
    click(
        &mut usage,
        at(9),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    let (range_request, query) = usage.session_usage_request().unwrap();
    assert_eq!(query.hour_starts_ms, selected(&[3, 4, 5, 6, 7, 8, 9]));
    usage.apply_session_usage_result(range_request, Ok(codex_report.clone()));
    append_dialog_render(
        &mut output,
        "Usage range added",
        120,
        36,
        &drawn(&mut usage, 120, 36),
    );
    // Deselecting the only selected hour is distinct from All.
    click(&mut usage, at(2), KeyModifiers::NONE);
    click(&mut usage, at(2), KeyModifiers::CONTROL);
    let (empty_request, query) = usage.session_usage_request().unwrap();
    assert_eq!(query.hour_starts_ms, selected(&[]));
    let mut empty_report = codex_report.clone();
    empty_report.cost_usd_ticks = Ok(0);
    empty_report.agent_usage.clear();
    empty_report.profile_usage.clear();
    for session in &mut empty_report.sessions {
        session.cost_usd_ticks = Ok(0);
        session.models.clear();
        session.coverage = Default::default();
    }
    for quota in &mut empty_report.quota_usage {
        quota.consumed_basis_points = Some(0);
        quota.partial = false;
    }
    usage.apply_session_usage_result(empty_request, Ok(empty_report));
    append_dialog_render(
        &mut output,
        "Usage no hours selected",
        120,
        36,
        &drawn(&mut usage, 120, 36),
    );
    usage.handle_key(key(KeyCode::Enter));
    assert_eq!(
        usage.session_usage_request().unwrap().1.hour_starts_ms,
        None
    );
    // Ctrl from All starts with every displayed graph hour.
    click(&mut usage, at(2), KeyModifiers::CONTROL);
    let (_, query) = usage.session_usage_request().unwrap();
    assert_eq!(
        query.hour_starts_ms,
        selected(&(0..24).filter(|i| *i != 2).collect::<Vec<_>>())
    );
    usage.handle_key(key(KeyCode::Enter));
    // A first Shift-click after reset establishes a new anchor.
    click(&mut usage, at(7), KeyModifiers::SHIFT);
    assert_eq!(
        usage.session_usage_request().unwrap().1.hour_starts_ms,
        selected(&[7])
    );
    click(&mut usage, at(2), KeyModifiers::NONE);
    // The arrow keys move from the clicked bar to its neighbour; Enter goes back to all hours.
    usage.handle_key(key(KeyCode::Left));
    assert_eq!(
        usage.session_usage_request().unwrap().1.hour_starts_ms,
        selected(&[1])
    );
    // Holding an arrow past the oldest hour stays on it instead of clearing the choice.
    for _ in 0..30 {
        usage.handle_key(key(KeyCode::Left));
    }
    assert_eq!(
        usage.session_usage_request().unwrap().1.hour_starts_ms,
        selected(&[0])
    );
    usage.handle_key(key(KeyCode::Enter));
    let (_, query) = usage.session_usage_request().unwrap();
    assert_eq!(query.hour_start_ms, None);
    assert_eq!(query.hour_starts_ms, None);
    // Enter resets the cursor immediately, even before another render.
    usage.handle_key(key(KeyCode::Left));
    assert_eq!(
        usage.session_usage_request().unwrap().1.hour_starts_ms,
        selected(&[22])
    );
    usage.handle_key(key(KeyCode::Enter));

    // A refreshed graph can expire selected hours and their range anchor.
    let oldest = ((column - 2 * pitch) as u16, base_row as u16);
    click(&mut usage, at(2), KeyModifiers::NONE);
    click(&mut usage, oldest, KeyModifiers::CONTROL);
    let (old_request, query) = usage.session_usage_request().unwrap();
    assert_eq!(query.hour_starts_ms, selected(&[0, 2]));
    let mut rolled = codex_report.clone();
    rolled.hours.remove(0);
    let mut newest = *rolled.hours.last().unwrap();
    newest.start_ms = hour(24);
    rolled.hours.push(newest);
    usage.apply_session_usage_result(old_request, Ok(rolled.clone()));
    let (replacement, query) = usage.session_usage_request().unwrap();
    assert_ne!(replacement, old_request);
    assert_eq!(query.hour_starts_ms, selected(&[2]));
    assert!(matches!(&usage.mode, Mode::SessionUsageReport(dialog) if dialog.report.is_none()));
    usage.apply_session_usage_result(old_request, Ok(codex_report.clone()));
    assert!(matches!(&usage.mode, Mode::SessionUsageReport(dialog) if dialog.report.is_none()));
    usage.apply_session_usage_result(replacement, Ok(rolled.clone()));
    assert_eq!(usage.session_usage_request().unwrap().0, replacement);
    assert!(matches!(&usage.mode, Mode::SessionUsageReport(dialog) if dialog.report.is_some()));
    drawn(&mut usage, 120, 36);
    // The expired anchor is cleared: Shift establishes a new singleton.
    click(&mut usage, at(2), KeyModifiers::SHIFT);
    assert_eq!(
        usage.session_usage_request().unwrap().1.hour_starts_ms,
        selected(&[3])
    );
    click(&mut usage, oldest, KeyModifiers::CONTROL);
    assert_eq!(
        usage.session_usage_request().unwrap().1.hour_starts_ms,
        selected(&[1, 3])
    );
    // Expiring the last member preserves an explicit empty selection.
    click(&mut usage, oldest, KeyModifiers::NONE);
    let (old_request, _) = usage.session_usage_request().unwrap();
    rolled.hours.remove(0);
    let mut newest = *rolled.hours.last().unwrap();
    newest.start_ms = hour(25);
    rolled.hours.push(newest);
    usage.apply_session_usage_result(old_request, Ok(rolled.clone()));
    let (replacement, query) = usage.session_usage_request().unwrap();
    assert_ne!(replacement, old_request);
    assert_eq!(query.hour_starts_ms, selected(&[]));
    assert!(matches!(&usage.mode, Mode::SessionUsageReport(dialog) if dialog.report.is_none()));
    usage.apply_session_usage_result(replacement, Ok(rolled));
    assert_eq!(usage.session_usage_request().unwrap().0, replacement);
    drawn(&mut usage, 120, 36);
    click(&mut usage, oldest, KeyModifiers::CONTROL);
    assert_eq!(
        usage.session_usage_request().unwrap().1.hour_starts_ms,
        selected(&[2])
    );
    usage.handle_key(key(KeyCode::Enter));
    let (all_request, _) = usage.session_usage_request().unwrap();
    usage.apply_session_usage_result(all_request, Ok(codex_report.clone()));
    // The full dashboard requires 60 columns; render the modal by itself to
    // exercise hidden-hour mapping at a width where bars actually disappear.
    let mut narrow_terminal = Terminal::new(TestBackend::new(28, 36)).unwrap();
    narrow_terminal
        .draw(|frame| crate::render::render_modal(frame, frame.area(), &mut usage))
        .unwrap();
    let narrow = buffer_lines(narrow_terminal.backend().buffer());
    let narrow_row = narrow.iter().position(|line| line.contains("$0 ")).unwrap();
    let last_bar = narrow[narrow_row]
        .chars()
        .enumerate()
        .filter_map(|(index, c)| "▁▂▃▄▅▆▇█░".contains(c).then_some(index))
        .last()
        .unwrap();
    click(
        &mut usage,
        (last_bar as u16, narrow_row as u16),
        KeyModifiers::NONE,
    );
    assert_eq!(
        usage.session_usage_request().unwrap().1.hour_starts_ms,
        selected(&[23])
    );
    usage.handle_key(key(KeyCode::Enter));
    drawn(&mut usage, 120, 36);
    usage.handle_key(key(KeyCode::BackTab));
    usage.handle_key(key(KeyCode::Char(' ')));
    let (weekly_request, query) = usage.session_usage_request().unwrap();
    assert!(query.include_suspended);
    let mut suspended = codex_report;
    suspended.sessions[0].suspended = true;
    usage.apply_session_usage_result(weekly_request, Ok(suspended));
    append_dialog_render(
        &mut output,
        "Usage weekly including suspended",
        120,
        36,
        &drawn(&mut usage, 120, 36),
    );
    // A short terminal drops the graph and keeps the rows.
    append_dialog_render(
        &mut output,
        "Usage short terminal",
        70,
        17,
        &drawn(&mut usage, 70, 17),
    );
    usage.apply_session_usage_result(weekly_request, Err("test store unavailable".into()));
    append_dialog_render(
        &mut output,
        "Usage report failure",
        120,
        36,
        &drawn(&mut usage, 120, 36),
    );
    usage.handle_key(key(KeyCode::Esc));
    assert!(usage.session_usage_request().is_none());
    usage.dispatch_command(crate::CommandId::SessionUsageReport);
    let (reopened, _) = usage.session_usage_request().unwrap();
    assert_ne!(weekly_request, reopened);
    usage.apply_session_usage_result(weekly_request, Ok(report));
    assert!(matches!(&usage.mode,Mode::SessionUsageReport(dialog) if dialog.report.is_none()));
    usage.handle_key(key(KeyCode::Esc));

    // The first new session asks the binary to read the host's Git identity;
    // asking again while that read runs starts nothing more.
    let mut identity = dashboard_with_session(running_session());
    identity.require_git_identity_check();
    let action = identity.dispatch_command(crate::CommandId::NewSessionWizard);
    writeln!(
        output,
        "new session before the Git identity check: {action:?}"
    )
    .unwrap();
    let action = identity.dispatch_command(crate::CommandId::NewSessionWizard);
    writeln!(output, "new session while the check runs: {action:?}").unwrap();
    writeln!(
        output,
        "notice while the check runs: {:?}",
        identity.notices.current()
    )
    .unwrap();
    identity.apply_git_identity_check(Ok(mj_client::daemon::GitIdentity {
        name: Some("Ada Lovelace".into()),
        ..Default::default()
    }));
    for character in "ada.example.com".chars() {
        identity.handle_key(key(KeyCode::Char(character)));
    }
    append_dialog_render(
        &mut output,
        "Git identity with the email typed",
        100,
        24,
        &drawn(&mut identity, 100, 24),
    );
    let action = identity.handle_key(key(KeyCode::Enter));
    writeln!(output, "action: {action:?}").unwrap();
    let action = identity.handle_key(key(KeyCode::Esc));
    writeln!(output, "escape while saving: {action:?}").unwrap();
    append_dialog_render(
        &mut output,
        "Git identity saving",
        100,
        24,
        &drawn(&mut identity, 100, 24),
    );
    identity.apply_git_identity_save(Err("Git email \"ada.example.com\" needs an @".into()));
    append_dialog_render(
        &mut output,
        "Git identity save failure keeps the fields",
        100,
        24,
        &drawn(&mut identity, 100, 24),
    );
    let action = identity.handle_key(key(KeyCode::Enter));
    writeln!(output, "action: {action:?}").unwrap();
    identity.apply_git_identity_save(Ok(mj_client::daemon::GitIdentity {
        name: Some("Ada Lovelace".into()),
        email: Some("ada@example.com".into()),
        per_directory: false,
    }));
    let followup = identity.take_git_identity_followup();
    writeln!(output, "follow-up after saving: {followup:?}").unwrap();
    writeln!(
        output,
        "notice after saving: {:?}",
        identity.notices.current()
    )
    .unwrap();
    assert!(matches!(identity.mode, Mode::New(_)));
    let action = identity.dispatch_command(crate::CommandId::NewSessionWizard);
    assert!(!matches!(action, DashboardAction::CheckGitIdentity));

    let mut skipped = dashboard_with_session(running_session());
    skipped.require_git_identity_check();
    skipped.dispatch_command(crate::CommandId::NewSessionWizard);
    skipped.apply_git_identity_check(Ok(Default::default()));
    let action = skipped.handle_key(key(KeyCode::Esc));
    writeln!(output, "skip: {action:?}").unwrap();
    let followup = skipped.take_git_identity_followup();
    writeln!(output, "follow-up after skipping: {followup:?}").unwrap();
    writeln!(
        output,
        "notice after skipping: {:?}",
        skipped.notices.current()
    )
    .unwrap();
    assert!(matches!(skipped.mode, Mode::New(_)));

    // A launch that bypasses the new-session key waits for the same check,
    // and runs even when the user moved on before the answer came back.
    let mut launch = dashboard_with_session(running_session());
    launch.require_git_identity_check();
    let recipe = DashboardAction::GoLaunch {
        recipe: mj_core::go::GoRecipe {
            profile_id: "codex".into(),
            target_id: "localhost".into(),
            bundle_id: None,
            project_directory: None,
            create_managed_worktree: None,
            subagents: None,
            additional_mounts: Vec::new(),
            resource_allocation: None,
        },
    };
    let action = launch.gate_launch_on_git_identity(recipe.clone());
    writeln!(output, "fast-start launch before the check: {action:?}").unwrap();
    launch.dispatch_command(crate::CommandId::SessionUsageReport);
    launch.apply_git_identity_check(Ok(Default::default()));
    assert_eq!(launch.take_git_identity_followup(), Some(recipe));
    writeln!(
        output,
        "notice when the check finds no identity behind another dialog: {:?}",
        launch.notices.current()
    )
    .unwrap();

    mj_core::golden::assert_golden(
        env!("CARGO_MANIFEST_DIR"),
        "dashboard-dialogs",
        output.trim_end_matches('\n'),
    );
}

#[test]
fn golden_tui_web_viewer_dialog() {
    let mut output = String::new();
    let url = "https://example.test/auth/login?token=secret";
    let qr = render_qr(url).expect("QR code");
    let qr_rows = qr
        .lines()
        .map(|line| {
            line.chars()
                .map(|character| if character == ' ' { '·' } else { character })
                .collect::<String>()
        })
        .collect::<Vec<_>>();
    append_dialog_render(
        &mut output,
        "QR module map; · marks a light module",
        qr_rows.first().map_or(0, |row| row.chars().count()) as u16,
        qr_rows.len() as u16,
        &qr_rows,
    );

    let no_qr = WebDialog {
        loading: false,
        viewer_url: Some("http://127.0.0.1:37650".to_owned()),
        viewer_code: Some("022160".to_owned()),
        fallback_reason: Some("automatic Tailscale detection is disabled".to_owned()),
        ..WebDialog::loading()
    };
    append_dialog_render(
        &mut output,
        "access details without QR",
        140,
        40,
        &draw_web_dialog(&no_qr, 140, 40),
    );

    let long_url = "https://a-very-long-machine-name.some-tailnet.ts.net:37650/viewer";
    let wrapped = WebDialog {
        loading: false,
        viewer_url: Some(long_url.to_owned()),
        viewer_code: Some("022160".to_owned()),
        fallback_reason: None,
        message: None,
        qr: Some(render_qr(long_url).unwrap()),
        ..WebDialog::loading()
    };
    append_dialog_render(
        &mut output,
        "long URL wrapped with QR",
        60,
        40,
        &draw_web_dialog(&wrapped, 60, 40),
    );

    let conflict = failed_web_dashboard();
    let Mode::Web(dialog) = &conflict.mode else {
        unreachable!()
    };
    for (width, height) in [(140, 40), (80, 24), (60, 20)] {
        append_dialog_render(
            &mut output,
            "port conflict and recovery actions",
            width,
            height,
            &draw_web_dialog(dialog, width, height),
        );
    }

    let output = output.trim_end_matches('\n');
    mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), "tui-web-viewer-dialog", output);
}

#[test]
fn golden_tui_container_editor_details() {
    let mut output = String::new();

    let mut cached_session = running_session();
    cached_session.resource_allocation =
        Some(mj_core::state::SessionResourceAllocation::Container {
            cpus: 4,
            memory_bytes: 8 << 30,
        });
    cached_session.build_cache = Some(mj_core::state::SessionBuildCache {
        host: "ssh:morannon".into(),
        directory: PathBuf::from("/mnt/nvme/mbx"),
        max_size: Some("1000GB".into()),
        target_root: None,
    });
    let mut cached = dashboard_with_session(cached_session);
    open_container_editor(&mut cached);
    append_dialog_render(
        &mut output,
        "session build cache",
        120,
        40,
        &drawn(&mut cached, 120, 40),
    );

    if let Mode::EditContainer(editor) = &mut cached.mode {
        editor.cpus = "1.5".into();
    }
    while container_editor(&cached).focused() != ContainerEditFocus::Save {
        cached.handle_key(key(KeyCode::Tab));
    }
    cached.handle_key(key(KeyCode::Enter));
    append_dialog_render(
        &mut output,
        "invalid fractional CPUs",
        120,
        40,
        &drawn(&mut cached, 120, 40),
    );

    let mut uncached = dashboard_with_session(running_session());
    open_container_editor(&mut uncached);
    append_dialog_render(
        &mut output,
        "session without build cache",
        120,
        40,
        &drawn(&mut uncached, 120, 40),
    );

    let mut mounted = dashboard_with_container_session();
    open_container_editor(&mut mounted);
    append_dialog_render(
        &mut output,
        "container settings can replace the container",
        100,
        40,
        &drawn(&mut mounted, 100, 40),
    );

    mounted.cancel_modal();
    mounted.focus_sessions();
    mounted.set_container_upgrade_sessions(["session-1".into()].into());
    let preview = mj_core::state::ContainerUpgradePreview {
        settings: None,
        session_id: "session-1".into(),
        source_container_id: "existing-container".into(),
        warning: mj_core::state::CONTAINER_UPGRADE_WARNING.into(),
        current_image: "ghcr.io/brokkai/mjolnir/session-base:2.51.0".into(),
        current_image_id: "sha256:previous".into(),
        target_image_id: Some("sha256:selected".into()),
        target_image: "ghcr.io/brokkai/mjolnir/session-base:2.52.0".into(),
        available: true,
        retained_mounts: vec!["/workspace".into(), "/cache".into(), "/tmp".into()],
    };
    assert_eq!(
        mounted.dispatch_command(crate::actions::CommandId::RestartSession),
        DashboardAction::PreviewContainerUpgrade {
            session_id: "session-1".into(),
            upgrade_only: false
        }
    );
    mounted.show_container_upgrade_preview(preview.clone(), false);
    append_dialog_render(
        &mut output,
        "Restart keeps its environment; container upgrade is a separate choice",
        120,
        32,
        &drawn(&mut mounted, 120, 32),
    );
    assert_eq!(
        mounted.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert!(matches!(mounted.mode, Mode::Dashboard));
    mounted.show_container_upgrade_preview(preview.clone(), false);
    drawn(&mut mounted, 120, 32);
    mounted.handle_key(key(KeyCode::Right));
    assert_eq!(
        mounted.handle_key(key(KeyCode::Enter)),
        DashboardAction::RestartSession {
            session_id: "session-1".into(),
            acknowledge_container_recreation: false
        }
    );
    mounted.show_container_upgrade_preview(preview.clone(), true);
    append_dialog_render(
        &mut output,
        "Explicit upgrade discloses image versions, retained storage, and writable-layer loss",
        120,
        32,
        &drawn(&mut mounted, 120, 32),
    );
    mounted.handle_key(key(KeyCode::Right));
    assert_eq!(
        mounted.handle_key(key(KeyCode::Enter)),
        DashboardAction::UpgradeContainer(mj_core::state::ContainerUpgradeRequest {
            selection: preview,
            acknowledge_writable_layer_loss: true
        })
    );
    mounted.set_container_upgrade_sessions(Default::default());
    assert_eq!(
        mounted.dispatch_command(crate::actions::CommandId::RestartSession),
        DashboardAction::RestartSession {
            session_id: "session-1".into(),
            acknowledge_container_recreation: false,
        }
    );

    mj_core::golden::assert_golden(
        env!("CARGO_MANIFEST_DIR"),
        "tui-container-editor-details",
        &output,
    );
}

#[test]
fn golden_tui_import_confirmation() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.show_import_bundle_confirmation(
        vec!["/work/repo — 1 tracked change · 2 untracked paths".into()],
        Vec::new(),
        vec!["/tmp/mj-golden-scratch".into()],
        mj_core::state::ManagedWorktreeOptions {
            available: true,
            default_create: true,
        },
        "/work/repo".into(),
    );
    let lines = drawn(&mut dashboard, 120, 30);
    let mut output = String::new();
    append_dialog_render(
        &mut output,
        "scratch repository excluded from workspace",
        120,
        30,
        &lines,
    );
    dashboard.show_import_progress("Imported session".into());
    dashboard.handle_key(key(KeyCode::Esc));
    assert!(matches!(dashboard.mode, Mode::Confirm(_)));
    dashboard.show_import_registration();
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Esc)),
        DashboardAction::None
    );
    assert!(matches!(dashboard.mode, Mode::Importing(_)));
    append_dialog_render(
        &mut output,
        "daemon registration cannot be cancelled",
        120,
        30,
        &drawn(&mut dashboard, 120, 30),
    );
    dashboard.show_import_upload();
    assert!(matches!(&dashboard.mode, Mode::Importing(progress) if progress.cancellable));
    dashboard.handle_key(key(KeyCode::Esc));
    assert!(matches!(dashboard.mode, Mode::Confirm(_)));
    dashboard.show_import_registration();
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Esc)),
        DashboardAction::None
    );
    assert!(matches!(&dashboard.mode, Mode::Importing(progress) if !progress.cancellable));
    mj_core::golden::assert_golden(
        env!("CARGO_MANIFEST_DIR"),
        "tui-import-confirmation",
        &output,
    );
}

#[test]
fn golden_tui_checkpoint_origin_dialog() {
    use std::fmt::Write as _;

    fn append_state(
        output: &mut String,
        label: &str,
        terminal: &mut Terminal<TestBackend>,
        width: u16,
        height: u16,
    ) {
        let cursor = terminal.get_cursor_position().expect("source cursor");
        let buffer = terminal.backend().buffer();
        let lines = buffer_lines(buffer);
        append_dialog_render(output, label, width, height, &lines);

        let source_row = lines
            .iter()
            .position(|line| line.contains("Source:"))
            .expect("source field row");
        let field_x = buffer.area.x + cell_column(&lines[source_row], "Source:") + 8;
        let source_y = buffer.area.y + source_row as u16;
        let source_focused = Some(buffer[(field_x, source_y)].bg) == theme::field(true).bg;
        let button_row = lines
            .iter()
            .position(|line| line.contains(" Cancel ") && line.contains(" Check origin "))
            .expect("origin action row");
        let button_y = buffer.area.y + button_row as u16;
        let cancel_x = buffer.area.x + cell_column(&lines[button_row], "Cancel");
        let check_x = buffer.area.x + cell_column(&lines[button_row], "Check origin");
        let buttons_selected = buffer[(cancel_x, button_y)].bg == theme::palette().selection
            && buffer[(check_x, button_y)].bg == theme::palette().selection;
        if source_focused {
            writeln!(output, "cursor: ({}, {})", cursor.x, cursor.y).unwrap();
        } else {
            writeln!(output, "cursor: hidden").unwrap();
        }
        writeln!(output, "source field focused: {source_focused}").unwrap();
        writeln!(output, "both actions selected: {buttons_selected}").unwrap();
        writeln!(
            output,
            "Cancel action focused: {}",
            buffer[(cancel_x, button_y)].bg == theme::palette().accent
        )
        .unwrap();
    }

    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.show_repository_origin_dialog(
        "session-1".into(),
        "bifrost".into(),
        "b41dc78".into(),
        "https://github.com/BrokkAi/bifrost.git".into(),
        "BrokkAi/bifrost-dev".into(),
        DashboardAction::None,
    );
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .unwrap();
    let mut output = String::new();
    append_state(
        &mut output,
        "focused replacement source",
        &mut terminal,
        100,
        24,
    );

    dashboard.handle_key(key(KeyCode::Tab));
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .unwrap();
    append_state(
        &mut output,
        "focus moves to origin actions",
        &mut terminal,
        100,
        24,
    );

    mj_core::golden::assert_golden(
        env!("CARGO_MANIFEST_DIR"),
        "tui-checkpoint-origin-dialog",
        &output,
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

// Hard-won: bda82985: Notice rows shifted when an age exceeded the fixed eight-cell column.
#[test]
fn the_notice_log_age_column_keeps_messages_aligned_past_one_minute() {
    let ages = super::render::notice_log_ages(&[44, 77, 3_700]);
    let widths = ages
        .iter()
        .map(|age| age.chars().count())
        .collect::<Vec<_>>();
    assert!(widths.iter().all(|&width| width == widths[0]), "{ages:?}");
    assert!(ages.iter().all(|age| age.ends_with("ago ")), "{ages:?}");
    assert!(ages[0].trim_start().starts_with("44s"), "{ages:?}");
}

#[test]
fn launch_failure_survives_notices_and_retries_original_settings_once() {
    let mut dashboard = dashboard_with_session(stopped_session());
    let retry = DashboardAction::CreateSession {
        oracle: None,
        oracle_choice: None,
        remember_container_size: false,
        subagents: None,
        review: None,
        create_managed_worktree: None,
        workspace_id: "original-workspace".into(),
        profile_id: "codex".into(),
        provider: None,
        bundle_id: "project".into(),
        project_directory: None,
        target_template_id: "docker".into(),
        additional_mounts: Vec::new(),
        resource_allocation: None,
        carried: Default::default(),
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
fn suspend_safe_fix_enters_pending_state_and_refresh_requires_confirmation() {
    let mut dashboard = dashboard_with_session(running_session());
    let request_id = 77;
    dashboard.mode = Mode::Confirm(ConfirmDialog::new(Confirmation::SuspendSession {
        session_id: "session-1".into(),
        request_id,
        child_warning_titles: Vec::new(),
        preflight: None,
        viewing_all_untracked: false,
        can_prompt: true,
        resolving_blockers: false,
        confirming_history_only: false,
    }));
    let preflight = mj_core::suspend_consent::SuspendPreflight {
        session_id: "session-1".into(),
        blockers: vec![mj_core::suspend_consent::SuspendBlocker {
            kind: "hidden_tracked_edits".into(),
            paths: vec!["file".into()],
            remedy: "Clear flags.".into(),
        }],
        ..Default::default()
    };
    dashboard.apply_suspend_preflight(request_id, preflight);
    let _ = drawn(&mut dashboard, 100, 28);
    let action = dashboard.handle_key(key(KeyCode::Char('f')));
    assert_eq!(
        action,
        DashboardAction::ResolveSuspendBlockers {
            session_id: "session-1".into(),
            request_id
        }
    );
    assert!(
        matches!(&dashboard.mode, Mode::Confirm(dialog) if matches!(&dialog.confirmation, Confirmation::SuspendSession { resolving_blockers: true,
                        confirming_history_only: false, preflight: None, .. }))
    );
    let action = dashboard.apply_suspend_preflight(
        request_id,
        mj_core::suspend_consent::SuspendPreflight {
            session_id: "session-1".into(),
            ..Default::default()
        },
    );
    assert_eq!(action, DashboardAction::None);
    assert!(
        matches!(&dashboard.mode, Mode::Confirm(dialog) if matches!(&dialog.confirmation, Confirmation::SuspendSession { resolving_blockers: false,
                        confirming_history_only: false, preflight: Some(_), .. }))
    );
}

#[test]
fn suspend_history_only_requires_second_confirmation_and_back_returns_to_blockers() {
    let mut dashboard = dashboard_with_session(running_session());
    let confirmation = Confirmation::SuspendSession {
        session_id: "session-1".into(),
        request_id: 77,
        child_warning_titles: vec!["child".into()],
        preflight: Some(mj_core::suspend_consent::SuspendPreflight {
            blockers: vec![mj_core::suspend_consent::SuspendBlocker {
                kind: "hidden_tracked_edits".into(),
                paths: vec!["file".into()],
                remedy: "Clear flags".into(),
            }],
            interrupting: true,
            ..Default::default()
        }),
        viewing_all_untracked: false,
        can_prompt: true,
        resolving_blockers: false,
        confirming_history_only: false,
    };
    dashboard.activate_confirmation_button(confirmation.clone(), 0);
    assert!(matches!(dashboard.mode, Mode::Dashboard));
    let index = confirmation_buttons(&confirmation)
        .iter()
        .position(|label| label == "Suspend (history only)…")
        .unwrap();
    assert_eq!(
        dashboard.activate_confirmation_button(confirmation, index),
        DashboardAction::None
    );
    let rendered = drawn(&mut dashboard, 100, 28).join(" ");
    assert!(rendered.contains("Discard workspace Git changes"));
    assert!(!rendered.contains("Save a recovery copy"));
    let Mode::Confirm(dialog) = &dashboard.mode else {
        panic!("confirmation")
    };
    assert_eq!(
        confirmation_buttons(&dialog.confirmation),
        vec!["Back", "Suspend with history only"]
    );
    let confirmation = dialog.confirmation.clone();
    dashboard.activate_confirmation_button(confirmation, 0);
    let Mode::Confirm(dialog) = &dashboard.mode else {
        panic!("blockers")
    };
    assert!(
        confirmation_buttons(&dialog.confirmation)
            .iter()
            .any(|label| label == "Suspend (history only)…")
    );
    let confirmation = dialog.confirmation.clone();
    dashboard.activate_confirmation_button(confirmation, index);
    let Mode::Confirm(dialog) = &dashboard.mode else {
        panic!("confirmation")
    };
    let confirmation = dialog.confirmation.clone();
    assert_eq!(
        dashboard.activate_confirmation_button(confirmation, 1),
        DashboardAction::Suspend {
            session_id: "session-1".into(),
            consent: mj_core::suspend_consent::SuspendConsent {
                history_only: true,
                ..Default::default()
            },
        }
    );
}

#[test]
fn usage_timeline_range_and_provider_interactions() {
    use crossterm::event::{MouseButton, MouseEventKind};
    use mj_core::usage::{
        HOUR_MS, HourlyUsage, ModelUsageEstimate, ProfileQuotaProjection, ProfileQuotaUsage,
        ProfileRoleUsageEstimate, RoleAssignmentChange, SessionUsageReport, UsageAgentRole,
    };
    let mut usage = dashboard_with_session(running_session());
    usage.begin_session_usage_report(None);
    let first = usage.session_usage_request().unwrap().0;
    let hours: Vec<_> = (0..24)
        .map(|i| HourlyUsage {
            start_ms: (490_000 + i) * HOUR_MS,
            known_cost_usd_ticks: 10_000_000_000,
            has_unknown_cost: false,
        })
        .collect();
    let report = SessionUsageReport {
        since_ms: hours[0].start_ms,
        until_ms: hours[23].start_ms + HOUR_MS,
        hours: hours.clone(),
        quota_usage: vec![ProfileQuotaUsage {
            profile: "codex".into(),
            consumed_basis_points: Some(2000),
            covered_seconds: Some(86_400),
            partial: false,
            monthly_price_usd: Some(100),
        }],
        quota_projections: vec![ProfileQuotaProjection {
            profile_id: "codex".into(),
            window_label: "5h".into(),
            is_short_window: true,
            remaining_percent: 1,
            resets_at_epoch_seconds: None,
            observed_pct_per_hour: None,
            sustainable_pct_per_hour: None,
            pace_ratio: None,
            projected_exhaustion_epoch_seconds: None,
        }],
        role_changes: vec![RoleAssignmentChange {
            id: 1,
            changed_at_ms: hours[8].start_ms + 1234,
            summary: "Reviewers changed".into(),
        }],
        ..Default::default()
    };
    usage.apply_session_usage_result(first, Ok(report.clone()));
    let screen = drawn(&mut usage, 120, 36);
    assert!(screen.iter().any(|line| line.contains("▲①")));
    let provider_y = screen.iter().position(|line| line.contains('▸')).unwrap();
    let provider_x = screen[provider_y].chars().position(|c| c == '▸').unwrap();
    let click =
        |usage: &mut DashboardState, p: (u16, u16), down: KeyModifiers, up: KeyModifiers| {
            for (kind, mods) in [
                (MouseEventKind::Down(MouseButton::Left), down),
                (MouseEventKind::Up(MouseButton::Left), up),
            ] {
                let mut event = mouse_at(kind, p);
                event.modifiers = mods;
                usage.handle_mouse(event);
            }
        };
    click(
        &mut usage,
        (provider_x as u16, provider_y as u16),
        KeyModifiers::NONE,
        KeyModifiers::NONE,
    );
    let screen = drawn(&mut usage, 120, 36);
    assert!(
        screen
            .iter()
            .any(|line| line.contains("External / untracked"))
    );
    assert!(!screen.iter().any(|line| line.contains("1% left")));
    usage.handle_key(key(KeyCode::Left));
    let screen = drawn(&mut usage, 120, 36);
    assert!(
        !screen
            .iter()
            .any(|line| line.contains("External / untracked"))
    );
    let mut with_roles = report.clone();
    with_roles.profile_role_usage = [(UsageAgentRole::Primary, 3), (UsageAgentRole::Reviewer, 1)]
        .into_iter()
        .map(|(role, cost)| ProfileRoleUsageEstimate {
            profile: Some("codex".into()),
            role,
            models: vec![ModelUsageEstimate {
                model: Some("model-a".into()),
                cost_usd_ticks: Ok(cost),
                ..Default::default()
            }],
            cost_usd_ticks: Ok(cost),
        })
        .collect();
    usage.apply_session_usage_result(first, Ok(with_roles));
    drawn(&mut usage, 120, 36);
    click(
        &mut usage,
        (provider_x as u16, provider_y as u16),
        KeyModifiers::NONE,
        KeyModifiers::NONE,
    );
    let expanded = drawn(&mut usage, 120, 36);
    assert!(expanded.iter().any(|line| line.contains("Primary agents")
        && line.contains("105%")
        && line.contains("75% share")));
    assert!(expanded.iter().any(|line| line.contains("Reviewers")
        && line.contains("35%")
        && line.contains("25% share")));
    usage.handle_key(key(KeyCode::Left));
    usage.apply_session_usage_result(first, Ok(report.clone()));
    let screen = drawn(&mut usage, 120, 36);
    let change_y = screen
        .iter()
        .position(|line| line.contains("Reviewers changed"))
        .unwrap();
    let change_x = screen[change_y].chars().position(|c| c == '①').unwrap();
    click(
        &mut usage,
        (change_x as u16, change_y as u16),
        KeyModifiers::NONE,
        KeyModifiers::NONE,
    );
    let (generation, query) = usage.session_usage_request().unwrap();
    assert_eq!(
        query.hour_starts_ms.unwrap(),
        hours[8..].iter().map(|h| h.start_ms).collect()
    );
    usage.apply_session_usage_result(generation, Ok(report.clone()));
    let screen = drawn(&mut usage, 120, 36);
    let bar_y = screen.iter().position(|line| line.contains("$0 ")).unwrap();
    let bar_x = screen[bar_y].chars().position(|c| c == '█').unwrap();
    let pitch = 4;
    let at = |i: usize| ((bar_x + i * pitch) as u16, bar_y as u16);
    click(&mut usage, at(2), KeyModifiers::NONE, KeyModifiers::NONE);
    let generation = usage.session_usage_request().unwrap().0;
    usage.apply_session_usage_result(generation, Ok(report.clone()));
    drawn(&mut usage, 120, 36);
    click(&mut usage, at(5), KeyModifiers::SHIFT, KeyModifiers::NONE);
    let (generation, query) = usage.session_usage_request().unwrap();
    assert_eq!(
        query.hour_starts_ms.unwrap(),
        hours[2..=5].iter().map(|h| h.start_ms).collect()
    );
    usage.apply_session_usage_result(generation, Ok(report.clone()));
    drawn(&mut usage, 120, 36);
    let mut shifted = key(KeyCode::Right);
    shifted.modifiers = KeyModifiers::SHIFT;
    usage.handle_key(shifted);
    let (generation, query) = usage.session_usage_request().unwrap();
    assert_eq!(
        query.hour_starts_ms.unwrap(),
        hours[2..=6].iter().map(|h| h.start_ms).collect()
    );
    usage.apply_session_usage_result(generation, Ok(report));
    drawn(&mut usage, 120, 36);
    usage.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), at(3)));
    usage.handle_mouse(mouse_at(MouseEventKind::Drag(MouseButton::Left), at(9)));
    assert_eq!(usage.session_usage_request().unwrap().0, generation);
    usage.handle_mouse(mouse_at(MouseEventKind::Up(MouseButton::Left), at(9)));
    let (after, query) = usage.session_usage_request().unwrap();
    assert_eq!(after, generation + 1);
    assert_eq!(
        query.hour_starts_ms.unwrap(),
        hours[3..=9].iter().map(|h| h.start_ms).collect()
    );
}

/// A drag across the hour chart only previews its range; releasing outside
/// the chart discards the preview without touching the report query.
// Hard-won: review-group-e0d8539b: the drag mutated the live query, and a release off the chart still committed it.
#[test]
fn usage_hour_drag_released_outside_the_chart_keeps_the_query() {
    use crossterm::event::{MouseButton, MouseEventKind};
    use mj_core::usage::{HOUR_MS, HourlyUsage, SessionUsageReport};
    let mut usage = dashboard_with_session(running_session());
    usage.begin_session_usage_report(None);
    let first = usage.session_usage_request().unwrap().0;
    let hours: Vec<_> = (0..24)
        .map(|i| HourlyUsage {
            start_ms: (490_000 + i) * HOUR_MS,
            known_cost_usd_ticks: 10_000_000_000,
            has_unknown_cost: false,
        })
        .collect();
    let report = SessionUsageReport {
        since_ms: hours[0].start_ms,
        until_ms: hours[23].start_ms + HOUR_MS,
        hours: hours.clone(),
        ..Default::default()
    };
    usage.apply_session_usage_result(first, Ok(report.clone()));
    let screen = drawn(&mut usage, 120, 36);
    let bar_y = screen.iter().position(|line| line.contains("$0 ")).unwrap() as u16;
    let bar_x = screen[usize::from(bar_y)]
        .chars()
        .position(|c| c == '█')
        .unwrap() as u16;
    let pitch = 4;
    let at = |i: usize| (bar_x + i as u16 * pitch, bar_y);
    usage.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), at(2)));
    usage.handle_mouse(mouse_at(MouseEventKind::Up(MouseButton::Left), at(2)));
    let (generation, query) = usage.session_usage_request().unwrap();
    let selected = query.hour_starts_ms.unwrap();
    assert_eq!(selected, [hours[2].start_ms].into_iter().collect());
    usage.apply_session_usage_result(generation, Ok(report));
    drawn(&mut usage, 120, 36);

    usage.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), at(3)));
    usage.handle_mouse(mouse_at(MouseEventKind::Drag(MouseButton::Left), at(9)));
    let (during, query) = usage.session_usage_request().unwrap();
    assert_eq!(during, generation, "a drag must not reload the report");
    assert_eq!(
        query.hour_starts_ms.as_ref(),
        Some(&selected),
        "a drag previews its range without changing the query"
    );
    usage.handle_mouse(mouse_at(
        MouseEventKind::Up(MouseButton::Left),
        (at(9).0, bar_y + 5),
    ));
    let (after, query) = usage.session_usage_request().unwrap();
    assert_eq!(
        after, generation,
        "a release outside the chart commits nothing"
    );
    assert_eq!(query.hour_starts_ms.as_ref(), Some(&selected));
}

/// Exhaustion ahead of the reset is a warning even at a "safe" weekly pace.
// Hard-won: review-group-e0d8539b: "dry in ..." at pace ≤ 105% rendered like "on pace · safe".
#[test]
fn usage_dry_forecast_at_a_safe_pace_is_styled_as_a_warning() {
    use mj_core::usage::{ProfileQuotaProjection, ProfileQuotaUsage, SessionUsageReport};
    let mut usage = dashboard_with_session(running_session());
    usage.begin_session_usage_report(None);
    let first = usage.session_usage_request().unwrap().0;
    let now = mj_core::clock::epoch_seconds() as i64;
    let report = SessionUsageReport {
        quota_usage: vec![
            // Anthropic sorts ahead of OpenAI so OpenAI is not the selected
            // row (row 0) whose selection highlight overrides cell foreground.
            ProfileQuotaUsage {
                profile: "claude".into(),
                consumed_basis_points: Some(5_000),
                covered_seconds: Some(604_800),
                partial: false,
                monthly_price_usd: Some(100),
            },
            ProfileQuotaUsage {
                profile: "codex".into(),
                consumed_basis_points: Some(10_000),
                covered_seconds: Some(604_800),
                partial: false,
                monthly_price_usd: Some(100),
            },
        ],
        quota_projections: vec![ProfileQuotaProjection {
            profile_id: "codex".into(),
            window_label: "Week".into(),
            is_short_window: false,
            remaining_percent: 10,
            resets_at_epoch_seconds: Some(now + 3 * 86_400),
            observed_pct_per_hour: None,
            sustainable_pct_per_hour: None,
            pace_ratio: None,
            projected_exhaustion_epoch_seconds: None,
        }],
        ..Default::default()
    };
    usage.apply_session_usage_result(first, Ok(report));
    let mut terminal = Terminal::new(TestBackend::new(120, 36)).expect("terminal");
    terminal
        .draw(|frame| render(frame, &mut usage))
        .expect("draw the surface");
    let buffer = terminal.backend().buffer();
    let (x, y) = point(&buffer_lines(buffer), "dry in");
    assert_eq!(buffer[(x, y)].fg, mj_chat::theme::palette().warning);
}
