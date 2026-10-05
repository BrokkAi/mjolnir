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
        "mount change takes effect on recreation",
        100,
        40,
        &drawn(&mut mounted, 100, 40),
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
        Vec::new(),
        Vec::new(),
        vec!["/tmp/mj-golden-scratch".into()],
        false,
        Default::default(),
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
        subagents: None,
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
