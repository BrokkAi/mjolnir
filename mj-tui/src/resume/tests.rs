use std::collections::BTreeMap;

use crossterm::event::KeyCode;
use mj_core::state::STATE_VERSION;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use super::*;
use crate::test_support::*;
use crate::{DashboardState, Focus};

/// Later than `stopped_session`'s checkpoint, so a native row built with
/// it sorts above the Hel record.
const NEWER_THAN_THE_CHECKPOINT: i64 = 4_000_000_000_000;

#[test]
fn managed_clone_resume_row_marks_unpublished_checkpoint_work() {
    let mut session = stopped_session();
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
    session.publication = Some(mj_core::state::PublicationAssessment {
        checkpoint_sha256: "a".repeat(64),
        state: PublicationState::Unpublished,
        dirty: false,
        stashed: false,
        saved_commits: vec!["2".repeat(40)],
        destinations: Vec::new(),
        checked_at: "2026-08-09T01:00:00Z".into(),
        reason: Some("master contains an unpublished commit".into()),
    });
    let rows = merged_resume_rows(&config(), &state_with(vec![session]), &[], &[]);
    let row = rows
        .iter()
        .find(|row| matches!(row.key, ResumeRowKey::Hel(_)))
        .unwrap();
    assert_eq!(row.publication, Some(PublicationState::Unpublished));
    assert!(row.details.contains("Unpublished work"));
    let line = resume_row_line(
        row,
        &RowLayout {
            profile: 12,
            origin: 16,
            activity: 12,
            title: 40,
        },
        &chrono::Utc::now(),
    );
    assert!(line.to_string().contains("↑ "));
}

fn native(id: &str, title: &str, last_activity_ms: i64) -> crate::ImportSessionOption {
    crate::ImportSessionOption {
        native_session_id: id.into(),
        title: title.into(),
        project_directory: "~/Projects/hel".into(),
        details: "master · 1.0KB · ~/Projects/hel".into(),
        unavailable_reason: None,
        last_activity_ms,
        natively_archived: false,
    }
}

fn codex_profile(sessions: Vec<crate::ImportSessionOption>) -> ImportProfileOption {
    ImportProfileOption {
        profile_id: "codex-1".into(),
        harness_kind: HarnessKind::Codex,
        sessions,
        scan_progress: Some((1, 1)),
        error: None,
    }
}

/// C-25: a long title gives way to the "[unavailable]" marker, so the marker
/// is always shown in full.
#[test]
fn a_long_unavailable_title_keeps_its_marker() {
    for width in [120, 80, 60] {
        let mut dashboard = DashboardState::new(config(), state_with(Vec::new()), BTreeMap::new());
        let mut session = native(
            "native-long",
            &"A very long imported conversation title ".repeat(6),
            NEWER_THAN_THE_CHECKPOINT,
        );
        session.unavailable_reason = Some("missing Git repo".into());
        dashboard.show_resume_dialog(1, vec![codex_profile(vec![session])]);
        switch_to_import(&mut dashboard);
        let rendered = drawn(&mut dashboard, width, 34).join("\n");
        assert!(
            rendered.contains("[unavailable]"),
            "width {width}:\n{rendered}"
        );
    }
}

#[test]
fn unavailable_import_explains_why_and_copies_its_id_without_closing() {
    use crossterm::event::{MouseButton, MouseEventKind};

    for width in [120, 60] {
        for reason in [
            "missing Git repo",
            "Legacy Codex history cannot be imported",
        ] {
            let mut dashboard =
                DashboardState::new(config(), state_with(Vec::new()), BTreeMap::new());
            let mut session = native(
                "native-unavailable",
                "Unavailable",
                NEWER_THAN_THE_CHECKPOINT,
            );
            session.unavailable_reason = Some(reason.into());
            dashboard.show_resume_dialog(
                1,
                vec![codex_profile(vec![
                    session,
                    native("native-ready", "Ready", 1),
                ])],
            );
            switch_to_import(&mut dashboard);
            let lines = drawn(&mut dashboard, width, 34);
            let rendered = lines.join("\n");
            let label = if reason == "missing Git repo" {
                "Cannot import: missing Git repo"
            } else {
                "Cannot import"
            };
            assert!(rendered.contains(label), "{rendered}");
            assert!(!rendered.contains("native-unavailable"), "{rendered}");
            assert!(!rendered.contains("Enter imports"), "{rendered}");
            assert_eq!(
                dashboard.handle_key(key(KeyCode::Enter)),
                DashboardAction::None
            );
            for kind in [
                MouseEventKind::Down(MouseButton::Left),
                MouseEventKind::Up(MouseButton::Left),
            ] {
                assert_eq!(
                    dashboard.handle_mouse(mouse_at(kind, point(&lines, label))),
                    DashboardAction::None
                );
            }
            assert!(matches!(dashboard.mode, Mode::ResumeDialog(_)));

            let copy = point(&lines, "Copy session ID");
            dashboard.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), copy));
            assert_eq!(
                dashboard.handle_mouse(mouse_at(MouseEventKind::Up(MouseButton::Left), copy)),
                DashboardAction::CopyNativeSessionId {
                    native_session_id: "native-unavailable".into()
                }
            );
            assert!(matches!(dashboard.mode, Mode::ResumeDialog(_)));

            // Tab navigation must reach the same copy action.
            if let Mode::ResumeDialog(dialog) = &mut dashboard.mode {
                dialog.form.get_mut().focus(ResumeFocus::Sessions);
            }
            dashboard.handle_key(key(KeyCode::Tab));
            dashboard.handle_key(key(KeyCode::Tab));
            assert_eq!(
                dashboard.handle_key(key(KeyCode::Enter)),
                DashboardAction::CopyNativeSessionId {
                    native_session_id: "native-unavailable".into()
                }
            );

            if let Mode::ResumeDialog(dialog) = &mut dashboard.mode {
                dialog.form.get_mut().focus(ResumeFocus::Sessions);
            }
            dashboard.handle_key(key(KeyCode::Down));
            dashboard.clear_notice();
            let ready = drawn(&mut dashboard, width, 34).join("\n");
            assert!(!ready.contains("Copy session ID"), "{ready}");
            assert!(!ready.contains("Cannot import"), "{ready}");
            assert_eq!(
                dashboard.handle_key(key(KeyCode::Enter)),
                DashboardAction::ImportSession {
                    profile_id: "codex-1".into(),
                    native_session_id: "native-ready".into(),
                    display_title: "Ready".into()
                }
            );
        }
    }
}

fn state_with(sessions: Vec<SessionRecord>) -> State {
    State {
        subagents: Default::default(),
        version: STATE_VERSION,
        sessions: sessions
            .into_iter()
            .map(|session| (session.id.clone(), session))
            .collect(),
        mount_history: BTreeMap::new(),
        container_sizes: BTreeMap::new(),
    }
}

fn incomplete_move() -> MoveOperation {
    MoveOperation {
        in_place: false,
        source_checkpoint_only: false,
        operation_id: "move-1".into(),
        selection: mj_core::state::MoveSelection {
            clear_resource_allocation: false,
            session_id: "session-1".into(),
            profile_id: Some("codex-1".into()),
            target_template_id: Some("target-1".into()),
            additional_mounts: None,
            resource_allocation: None,
        },
        source_profile_id: "codex-1".into(),
        source_target_template_id: "target-1".into(),
        source_target: None,
        source_native_session_id: None,
        source_additional_mounts: Vec::new(),
        source_resource_allocation: None,
        destination_target: None,
        destination_native_session_id: None,
        destination_store_id: None,
        configuration_fingerprint: "fingerprint".into(),
        checkpoint: None,
        recovery_session: None,
        queue: mj_core::state::ResumeQueueDisposition::Start,
        phase: mj_core::state::MovePhase::Cancelled,
        queue_admission_started: true,
        queue_admission_finished: false,
        cancellation_requested: true,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
        error: Some("queue admission interrupted".into()),
    }
}

fn rows(dashboard: &DashboardState) -> Vec<ResumeRow> {
    assert!(
        matches!(dashboard.mode, Mode::ResumeDialog(_)),
        "expected the resume dialog"
    );
    dashboard.resume_rows().to_vec()
}

fn titles(rows: &[ResumeRow]) -> Vec<&str> {
    rows.iter().map(|row| row.title.as_str()).collect()
}

fn dialog_focus(dashboard: &DashboardState) -> ResumeFocus {
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    dialog.focused()
}

#[test]
fn incomplete_move_resume_row_opens_same_destination_retry_controls() {
    let mut dashboard = DashboardState::new(
        config(),
        state_with(vec![stopped_session()]),
        BTreeMap::new(),
    );
    dashboard.set_move_operations([incomplete_move()]);
    dashboard.show_resume_dialog(1, Vec::new());
    switch_to_hel(&mut dashboard);

    let row = dashboard
        .resume_rows()
        .iter()
        .find(|row| row.session_id() == Some("session-1"))
        .expect("stopped session remains in resume rows");
    assert!(row.move_recovery.is_some());
    assert_eq!(
        crate::dialogs::confirmation_buttons(&Confirmation::RecoverMove {
            operation: Box::new(incomplete_move()),
        }),
        &["Cancel", "Open transcript", "Retry move"]
    );
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert!(matches!(
        dashboard.mode,
        Mode::Confirm(ConfirmDialog {
            confirmation: Confirmation::RecoverMove { .. },
            ..
        })
    ));
}

fn replace_search(dashboard: &mut DashboardState, search: &str) {
    let Mode::ResumeDialog(dialog) = &mut dashboard.mode else {
        panic!("expected the resume dialog");
    };
    dialog.search = search.to_owned().into();
    dashboard.rebuild_resume_rows();
}

/// Tabs forward until `control` has keyboard focus.
fn focus_resume_control(dashboard: &mut DashboardState, control: ResumeFocus) {
    for _ in 0..8 {
        let Mode::ResumeDialog(dialog) = &dashboard.mode else {
            panic!("expected the resume dialog");
        };
        if dialog.focused() == control {
            return;
        }
        dashboard.handle_key(key(KeyCode::Tab));
    }
    panic!("{control:?} never received focus");
}

fn switch_to_import(dashboard: &mut DashboardState) {
    switch_to_tab(dashboard, ResumeTab::Import);
}

/// Move the dialog to a tab by name, through the tab strip.
fn switch_to_tab(dashboard: &mut DashboardState, tab: ResumeTab) {
    for _ in 0..=ResumeTab::COUNT {
        let Mode::ResumeDialog(dialog) = &mut dashboard.mode else {
            panic!("expected the resume dialog");
        };
        if dialog.tab == tab {
            if dialog.focused() == ResumeFocus::Tabs {
                dashboard.handle_key(key(KeyCode::Enter));
            }
            return;
        }
        let forward = tab.index() > dialog.tab.index();
        dialog.form.get_mut().focus(ResumeFocus::Tabs);
        dashboard.handle_key(key(if forward {
            KeyCode::Right
        } else {
            KeyCode::Left
        }));
    }
    panic!("the dialog never reached {tab:?}");
}

fn switch_to_hel(dashboard: &mut DashboardState) {
    switch_to_tab(dashboard, ResumeTab::Hel);
}

fn switch_to_archive(dashboard: &mut DashboardState) {
    switch_to_tab(dashboard, ResumeTab::Archive);
}

#[test]
fn last_active_uses_words_through_seven_days_then_a_local_date() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-08-23T12:00:00-05:00").unwrap();
    let before = |milliseconds| now.timestamp_millis() - milliseconds;

    assert_eq!(format_last_active(&now, before(30_000)), "just now");
    assert_eq!(format_last_active(&now, before(60_000)), "1 minute ago");
    assert_eq!(
        format_last_active(&now, before(2 * 60_000)),
        "2 minutes ago"
    );
    assert_eq!(format_last_active(&now, before(60 * 60_000)), "1 hour ago");
    assert_eq!(
        format_last_active(&now, before(24 * 60 * 60_000)),
        "1 day ago"
    );
    assert_eq!(
        format_last_active(&now, before(SEVEN_DAYS_MS)),
        "7 days ago"
    );
    assert_eq!(
        format_last_active(&now, before(SEVEN_DAYS_MS + 1)),
        "Aug 16, 2026"
    );
    assert_eq!(
        format_last_active(&now, before(9 * 24 * 60 * 60_000)),
        "Aug 14, 2026"
    );
    assert_eq!(
        format_last_active(&now, now.timestamp_millis() + 1),
        "just now"
    );
    assert_eq!(format_last_active(&now, 0), "unknown");
    assert_eq!(format_last_active(&now, i64::MAX), "unknown");
}

/// A Hel record and the native session it was imported from are one
/// conversation, so the dialog shows the Hel record's row and drops the
/// native duplicate.
#[test]
fn a_hel_record_replaces_the_native_session_it_was_imported_from() {
    // `stopped_session` carries native_session_id "native-1".
    let dashboard = DashboardState::new(
        config(),
        state_with(vec![stopped_session()]),
        BTreeMap::new(),
    );
    let merged = merged_resume_rows(
        &dashboard.config,
        &dashboard.state,
        &[codex_profile(vec![
            native("native-1", "Same conversation", 10),
            native("native-2", "A different conversation", 5),
        ])],
        &[],
    );

    assert_eq!(merged.len(), 2, "{:?}", titles(&merged));
    let adopted = merged
        .iter()
        .find(|row| row.key == ResumeRowKey::Hel("session-1".into()))
        .expect("the hel record keeps its row");
    assert_eq!(adopted.title, "ACP pretty name");
    assert_eq!(adopted.origin, "podman");
    assert!(
        !merged
            .iter()
            .any(|row| row.key == ResumeRowKey::Native(HarnessKind::Codex, "native-1".into())),
        "the native duplicate is gone"
    );
    // A native session with no Hel record still shows, marked local.
    let native_only = merged
        .iter()
        .find(|row| row.key == ResumeRowKey::Native(HarnessKind::Codex, "native-2".into()))
        .expect("the unadopted native session keeps its row");
    assert_eq!(native_only.origin, "local/hel");
}

#[test]
fn resume_and_import_targets_include_the_project_like_live_summaries() {
    let mut local = stopped_session();
    local.id = "local-session".into();
    local.native_session_id = None;
    local.target_template_id = "localhost".into();
    local.project_directory = Some("/mnt/optane/bifrost-fird".into());

    let mut remote = stopped_session();
    remote.id = "remote-session".into();
    remote.native_session_id = None;
    remote.target_template_id = "precision-3260".into();
    remote.project_directory = Some("/home/jonathan/Projects/bifrost".into());

    let mut config = config();
    config.targets.insert(
        "localhost".into(),
        mj_core::config::TargetTemplate::LocalBare,
    );
    config.targets.insert(
        "precision-3260".into(),
        mj_core::config::TargetTemplate::SshBare {
            ssh: mj_core::config::SshConnection {
                host: "precision-3260".into(),
                user: None,
                identity_file: None,
                extra_args: Vec::new(),
            },
            permissions: mj_core::config::PermissionMode::Yolo,
            workspace_prefix: ".local/share/hel/workspaces".into(),
        },
    );
    let mut dashboard =
        DashboardState::new(config, state_with(vec![local, remote]), BTreeMap::new());
    dashboard.show_resume_dialog(
        1,
        vec![codex_profile(vec![native(
            "native-only",
            "Native project",
            NEWER_THAN_THE_CHECKPOINT,
        )])],
    );
    switch_to_hel(&mut dashboard);

    let targets = rows(&dashboard)
        .into_iter()
        .map(|row| row.origin)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        targets,
        BTreeSet::from([
            "localhost/bifrost-fird".to_owned(),
            "precision-3260/bifrost".to_owned(),
        ])
    );

    let mut terminal = Terminal::new(TestBackend::new(140, 34)).expect("terminal");
    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .expect("draw the resume dialog");
    let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
    for target in ["localhost/bifrost-fird", "precision-3260/bifrost"] {
        assert!(rendered.contains(target), "{rendered}");
    }

    switch_to_import(&mut dashboard);
    assert_eq!(rows(&dashboard)[0].origin, "local/hel");
}

/// The native session behind a live Hel session must not be offered as an
/// import: that would start a second Hel session on the same conversation.
#[test]
fn a_live_session_hides_its_native_counterpart_from_the_dialog() {
    let mut live = stopped_session();
    live.state = SessionState::Running;
    let merged = merged_resume_rows(
        &config(),
        &state_with(vec![live]),
        &[codex_profile(vec![
            native("native-1", "Running under Hel right now", 10),
            native("native-2", "Idle native session", 5),
        ])],
        &[],
    );
    assert_eq!(titles(&merged), ["Idle native session"]);
}

/// A record whose target was renamed or removed from config still reports
/// the target it actually ran on.
#[test]
fn the_origin_chip_shows_the_stored_target_even_when_config_forgot_it() {
    let mut session = stopped_session();
    session.target_template_id = "retired-target".into();
    let mut config = config();
    config.targets.clear();
    let merged = merged_resume_rows(&config, &state_with(vec![session]), &[], &[]);
    assert_eq!(merged[0].origin, "retired-target");
}

/// One order across the merged list: newest activity first, whichever
/// source the row came from. Hel records date from their checkpoint,
/// native sessions from the file's modification time.
#[test]
fn rows_sort_by_last_activity_descending_across_both_sources() {
    let mut old_record = stopped_session();
    old_record.id = "old-record".into();
    old_record.native_session_id = None;
    old_record.checkpoint.as_mut().unwrap().created_at = "2026-01-01T00:00:00Z".into();
    let mut new_record = stopped_session();
    new_record.id = "new-record".into();
    new_record.native_session_id = None;
    new_record.acp_session_title = Some("Newest record".into());
    new_record.checkpoint.as_mut().unwrap().created_at = "2026-06-01T00:00:00Z".into();

    let january = 1_767_225_600_000; // 2026-01-01T00:00:00Z
    let march = 1_772_409_600_000; // 2026-03-01T00:00:00Z
    let july = 1_782_950_400_000; // 2026-07-01T00:00:00Z
    let merged = merged_resume_rows(
        &config(),
        &state_with(vec![old_record, new_record]),
        &[codex_profile(vec![
            native("native-mid", "Native March", march),
            native("native-new", "Native July", july),
        ])],
        &[],
    );

    assert_eq!(
        titles(&merged),
        [
            "Native July",
            "Newest record",
            "Native March",
            "ACP pretty name",
        ]
    );
    assert_eq!(merged[3].last_activity_ms, january);
}

/// A record archived by an older Mjolnir version remains part of stopped
/// history. Archive state is retained in storage for compatibility, but
/// it no longer separates rows in the resume dialog.
#[test]
fn previously_archived_history_remains_visible() {
    let mut archived_record = stopped_session();
    archived_record.id = "archived-record".into();
    archived_record.native_session_id = None;
    archived_record.acp_session_title = Some("Archived record".into());
    archived_record.archived = true;
    let mut current_record = stopped_session();
    current_record.id = "current-record".into();
    current_record.native_session_id = None;
    current_record.acp_session_title = Some("Current record".into());

    let mut dashboard = DashboardState::new(
        config(),
        state_with(vec![archived_record, current_record]),
        BTreeMap::new(),
    );
    dashboard.show_resume_dialog(1, Vec::new());
    switch_to_hel(&mut dashboard);

    assert_eq!(
        titles(&rows(&dashboard)),
        ["Archived record", "Current record"]
    );
}

/// Native archive metadata is retained on the row for display consumers,
/// while the resume dialog always lists the row and never writes back to
/// the provider.
#[test]
fn native_archive_metadata_is_informational() {
    let mut natively_archived = native("native-codex", "Archived in Codex", 1);
    natively_archived.natively_archived = true;
    let mut dashboard = DashboardState::new(config(), state_with(Vec::new()), BTreeMap::new());
    dashboard.show_resume_dialog(1, vec![codex_profile(vec![natively_archived])]);
    switch_to_import(&mut dashboard);
    assert_eq!(titles(&rows(&dashboard)), ["Archived in Codex"]);
    assert!(rows(&dashboard)[0].natively_archived);
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Char('a'))),
        DashboardAction::None
    );
    assert_eq!(titles(&rows(&dashboard)), ["Archived in Codex"]);
}

/// A lost or force-destroyed session cannot be resumed; deleting its
/// record is the only thing left to do with it.
#[test]
fn lost_and_destroyed_rows_are_marked_and_refuse_to_resume() {
    for (state, marker, reason) in [
        (
            SessionState::Lost,
            "⚠ lost",
            "lost without a verified checkpoint",
        ),
        (
            SessionState::DestroyedWithDataLoss,
            "⚠ data lost",
            "force-destroyed",
        ),
    ] {
        let mut session = stopped_session();
        session.state = state;
        let mut dashboard =
            DashboardState::new(config(), state_with(vec![session]), BTreeMap::new());
        dashboard.show_resume_dialog(1, Vec::new());
        switch_to_hel(&mut dashboard);

        let row = &rows(&dashboard)[0];
        assert!(!row.status.is_recoverable());
        assert_eq!(row.status.warning(), Some(marker));
        assert!(row.details.contains(reason), "{}", row.details);

        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::None
        );
        assert!(matches!(dashboard.mode, Mode::ResumeDialog(_)));
        let notice = dashboard.notices.current().unwrap_or_default();
        assert!(notice.contains(reason), "{notice}");
        assert!(notice.contains("Use Destroy"), "{notice}");

        focus_resume_control(&mut dashboard, ResumeFocus::Destroy);
        dashboard.handle_key(key(KeyCode::Enter));
        assert!(matches!(dashboard.mode, Mode::Confirm(_)));
    }
}

/// The letter key used to destroy; that job now belongs to the Destroy
/// button, which reaches the same confirmation by keyboard or mouse.
#[test]
fn the_destroy_button_replaces_the_d_key() {
    let mut dashboard = DashboardState::new(
        config(),
        state_with(vec![stopped_session()]),
        BTreeMap::new(),
    );
    dashboard.show_resume_dialog(1, Vec::new());
    switch_to_hel(&mut dashboard);

    assert_eq!(
        dashboard.handle_key(key(KeyCode::Char('d'))),
        DashboardAction::None
    );
    assert!(matches!(dashboard.mode, Mode::ResumeDialog(_)));

    let mut terminal = Terminal::new(TestBackend::new(120, 34)).expect("terminal");
    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .expect("draw the resume dialog");
    let lines = buffer_lines(terminal.backend().buffer());
    let (row, line) = lines
        .iter()
        .enumerate()
        .find(|(_, line)| line.contains("  Destroy  "))
        .expect("Destroy button between Cancel and Resume");
    let cancel = cell_column(line, "Cancel");
    let destroy = cell_column(line, "Destroy");
    let resume = cell_column(line, "Resume");
    assert!(cancel < destroy && destroy < resume, "{line}");
    for kind in [
        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
    ] {
        dashboard.handle_mouse(crossterm::event::MouseEvent {
            kind,
            column: destroy,
            row: row as u16,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
    }
    let Mode::Confirm(confirm) = &dashboard.mode else {
        panic!(
            "expected the destroy confirmation, got {:?}",
            dashboard.mode
        );
    };
    assert!(matches!(
        confirm.confirmation,
        Confirmation::DestroyStopped { .. }
    ));
}

/// The active tab is highlighted whether or not the strip has focus, so
/// the current tab is visible at a glance.
#[test]
fn the_active_tab_is_highlighted_without_focus() {
    let mut dashboard = DashboardState::new(
        config(),
        state_with(vec![stopped_session()]),
        BTreeMap::new(),
    );
    dashboard.show_resume_dialog(1, Vec::new());
    switch_to_hel(&mut dashboard);
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert_eq!(dialog.focused(), ResumeFocus::Sessions);

    let mut terminal = Terminal::new(TestBackend::new(120, 34)).expect("terminal");
    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .expect("draw the resume dialog");
    let buffer = terminal.backend().buffer();
    let lines = buffer_lines(buffer);
    let row = lines
        .iter()
        .position(|line| line.contains(" Mjolnir ") && line.contains(" Import "))
        .expect("tab strip");
    let y = buffer.area.y + row as u16;
    let active = buffer.area.x + cell_column(&lines[row], "Mjolnir");
    let inactive = buffer.area.x + cell_column(&lines[row], "Import");
    assert_eq!(buffer[(active, y)].bg, theme::palette().accent);
    assert_eq!(buffer[(inactive, y)].bg, theme::palette().surface_raised);
}

/// Hel never modifies a harness home, so a native-only row has no destroy action.
#[test]
fn a_native_only_row_cannot_be_destroyed_from_hel() {
    let mut dashboard = DashboardState::new(config(), state_with(Vec::new()), BTreeMap::new());
    dashboard.show_resume_dialog(
        1,
        vec![codex_profile(vec![native(
            "native-2",
            "Native",
            NEWER_THAN_THE_CHECKPOINT,
        )])],
    );
    switch_to_import(&mut dashboard);

    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert!(!dialog.form.borrow().is_enabled(ResumeFocus::Destroy));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Delete)),
        DashboardAction::None
    );
    assert!(matches!(dashboard.mode, Mode::ResumeDialog(_)));
    assert!(
        dashboard
            .notices
            .current()
            .unwrap_or_default()
            .contains("never destroys")
    );
}

/// The default Hel tab and the Import tab each expose only the source they
/// name, with a valid selection after every switch.
fn wiki_row(id: &str, archived: bool) -> WikiRow {
    WikiRow {
        id: id.into(),
        tool: "mjolnir".into(),
        project: "/home/dev/project".into(),
        title: format!("archived {id}"),
        started: Some("2026-09-01T00:00:00Z".into()),
        msgs: 7,
        preview: Some("the last thing it said".into()),
        archived,
        native_id: None,
        snippet: None,
        hel_session_id: None,
        target: None,
        profile: None,
        harness: None,
    }
}

/// One answer from an index that has finished building and is not syncing.
fn ready_page(rows: Vec<WikiRow>) -> WikiSearchPage {
    WikiSearchPage {
        rows,
        status: WikiStatus {
            state: WikiIndexState::Ready,
            topping_up: false,
        },
    }
}

/// Put the open dialog in the state one ready answer would leave it in,
/// without going through a request: the tests that only care about rows
/// should not have to spell the status out.
fn apply_ready_rows(dashboard: &mut DashboardState, rows: Vec<WikiRow>) {
    let request_id = match &dashboard.mode {
        Mode::ResumeDialog(dialog) => dialog.wiki_request_id,
        _ => panic!("expected the resume dialog"),
    };
    dashboard.apply_wiki_search(request_id, ready_page(rows));
}

/// The dialog opens on Live with the session list itself focused, not the
/// search box, so a state letter narrows the list at once rather than being
/// typed into the box.
#[test]
fn the_dialog_opens_with_the_list_focused_and_a_letter_narrows_it_at_once() {
    let mut dashboard = dashboard_with_live_attention_mix();
    dashboard.show_resume_dialog(1, Vec::new());
    assert_eq!(dialog_focus(&dashboard), ResumeFocus::Sessions);

    drawn(&mut dashboard, 120, 40);
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Char('b'))),
        DashboardAction::None
    );
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert!(
        dialog.search.is_empty(),
        "the letter narrowed the list; the search box was never touched"
    );
    assert_eq!(
        rows(&dashboard)
            .into_iter()
            .map(|row| row.key)
            .collect::<Vec<_>>(),
        vec![ResumeRowKey::Live("live-remote".into())],
        "only the session waiting on a question counts as blocked"
    );
}

/// `/` still moves the focus to the search box, and once it has it, typing
/// narrows the Live list by name.
#[test]
fn slash_moves_focus_to_the_search_box_and_typing_narrows_by_name() {
    let mut dashboard = dashboard_with_live_sessions_in_two_workspaces();
    dashboard.show_resume_dialog(1, Vec::new());
    drawn(&mut dashboard, 120, 40);

    assert_eq!(
        dashboard.handle_key(key(KeyCode::Char('/'))),
        DashboardAction::None
    );
    assert_eq!(dialog_focus(&dashboard), ResumeFocus::Search);

    for character in "mast".chars() {
        dashboard.handle_key(key(KeyCode::Char(character)));
    }
    assert_eq!(titles(&rows(&dashboard)), ["Raise the mast"]);
}

/// C-12: when a state filter empties the Live list, the message names the
/// filter rather than saying nothing is running.
#[test]
fn an_empty_filtered_live_list_names_the_filter() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.show_resume_dialog(1, Vec::new());
    dashboard.handle_key(key(KeyCode::Char('b')));
    assert!(rows(&dashboard).is_empty(), "no session is blocked");
    let rendered = drawn(&mut dashboard, 120, 40).join("\n");
    assert!(rendered.contains("No blocked sessions"), "{rendered}");
    assert!(!rendered.contains("No running sessions"), "{rendered}");
}

/// C-13: the archived briefing's UTC date is shown in local time, like every
/// other time on screen.
#[test]
fn the_briefing_date_is_shown_in_local_time() {
    let central = chrono::FixedOffset::west_opt(5 * 3600).unwrap();
    assert_eq!(
        localize_brief_date(
            "- Tool: mjolnir | Project: /tmp/project | Date: 2026-09-23 18:46",
            &central
        ),
        "- Tool: mjolnir | Project: /tmp/project | Date: 2026-09-23 13:46"
    );
    let with_source = "- Tool: codex | Project: p | Date: 2026-09-24 02:00\n";
    assert_eq!(
        localize_brief_date(with_source.trim_end(), &central),
        "- Tool: codex | Project: p | Date: 2026-09-23 21:00"
    );
    for untouched in [
        "- Tool: codex | Project: p | Date: -",
        "Date: 2026-09-23 18:46",
    ] {
        assert_eq!(localize_brief_date(untouched, &central), untouched);
    }
}

/// C-6: Enter in the search box does what Enter on the list does. On the Live
/// tab that jumps to the matched session and closes the dialog.
#[test]
fn enter_in_the_search_box_opens_the_live_match() {
    let mut dashboard = dashboard_with_live_sessions_in_two_workspaces();
    dashboard.show_resume_dialog(1, Vec::new());
    drawn(&mut dashboard, 120, 40);

    dashboard.handle_key(key(KeyCode::Char('/')));
    for character in "mast".chars() {
        dashboard.handle_key(key(KeyCode::Char(character)));
    }
    assert_eq!(titles(&rows(&dashboard)), ["Raise the mast"]);
    drawn(&mut dashboard, 120, 40);
    dashboard.handle_key(key(KeyCode::Enter));
    assert!(
        matches!(dashboard.mode, Mode::Dashboard),
        "Enter on the search match left the dialog open"
    );
}

/// No production path opens the dialog anywhere but Live (see Milestone 5),
/// so `search_focus_pending` served no purpose and was removed. An index
/// answer arriving while the dialog is open must still leave the focus
/// wherever the person put it.
#[test]
fn an_index_answer_leaves_the_focused_control_unchanged() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.show_resume_dialog(1, Vec::new());
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert_ne!(dialog.wiki_status.state, WikiIndexState::Ready);
    let focus_before = dialog_focus(&dashboard);

    apply_ready_rows(&mut dashboard, Vec::new());
    assert_eq!(dialog_focus(&dashboard), focus_before);
}

/// A row is archived only when nothing on this machine still holds the
/// session: no Mjolnir record and no native file an import could adopt.
#[test]
fn only_indexed_sessions_nothing_else_holds_become_archived_rows() {
    let config = config();
    let state = state_with(vec![stopped_session()]);
    let profiles = [codex_profile(vec![native(
        "native-2",
        "Native",
        NEWER_THAN_THE_CHECKPOINT,
    )])];

    let gone = wiki_row("gone", true);
    let held = WikiRow {
        hel_session_id: Some("session-1".into()),
        snippet: Some("the phrase that matched".into()),
        ..wiki_row("held", true)
    };
    let imported = WikiRow {
        tool: "codex".into(),
        native_id: Some("native-2".into()),
        snippet: Some("a native hit".into()),
        ..wiki_row("imported", true)
    };
    let live_elsewhere = wiki_row("still-there", false);

    let merged = merged_resume_rows(
        &config,
        &state,
        &profiles,
        &[gone, held, imported, live_elsewhere],
    );
    let archived = merged
        .iter()
        .filter(|row| matches!(row.key, ResumeRowKey::Archive(_)))
        .collect::<Vec<_>>();
    assert_eq!(
        archived
            .iter()
            .map(|row| row.key.clone())
            .collect::<Vec<_>>(),
        vec![ResumeRowKey::Archive("gone".into())],
        "only the session nothing else holds is archived"
    );
    assert_eq!(archived[0].status, ResumeRowStatus::Restorable);

    let hel = merged
        .iter()
        .find(|row| row.key == ResumeRowKey::Hel("session-1".into()))
        .expect("the Mjolnir record is still listed");
    assert_eq!(hel.wiki_id(), Some("held"));
    assert!(
        hel.details.contains("the phrase that matched"),
        "a hit on a live record shows its snippet: {}",
        hel.details
    );
    let native_row = merged
        .iter()
        .find(|row| matches!(row.key, ResumeRowKey::Native(..)))
        .expect("the importable session is still listed");
    assert_eq!(native_row.wiki_id(), Some("imported"));
    assert!(native_row.details.contains("a native hit"));
}

/// The Archived tab shows archived rows only, and Enter on one opens the
/// wizard that restores it rather than resuming a record.
#[test]
fn the_archived_tab_lists_indexed_sessions_and_enter_restores_one() {
    let mut dashboard = DashboardState::new(
        config(),
        state_with(vec![stopped_session()]),
        BTreeMap::new(),
    );
    dashboard.show_resume_dialog(1, vec![codex_profile(Vec::new())]);
    // The dialog asks for the recent list as it opens, the way the
    // dashboard does, and the answer names that request.
    let (request_id, query) = dashboard.next_wiki_search().expect("a search is asked for");
    assert_eq!(query, "");
    dashboard.apply_wiki_search(request_id, ready_page(vec![wiki_row("gone", true)]));

    switch_to_hel(&mut dashboard);
    assert_eq!(titles(&rows(&dashboard)), ["ACP pretty name"]);
    switch_to_import(&mut dashboard);
    assert!(rows(&dashboard).is_empty());

    if let Mode::ResumeDialog(dialog) = &mut dashboard.mode {
        dialog.form.get_mut().focus(ResumeFocus::Sessions);
    }
    assert!(matches!(
        dashboard.handle_key(key(KeyCode::Right)),
        DashboardAction::LoadArchivedBrief { .. } | DashboardAction::None
    ));
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert_eq!(dialog.tab, ResumeTab::Archive);
    assert_eq!(titles(&rows(&dashboard)), ["archived gone"]);
    assert_eq!(dialog.selected, Some(ResumeRowKey::Archive("gone".into())));

    dashboard.handle_key(key(KeyCode::Enter));
    let Mode::Resume(wizard) = &dashboard.mode else {
        panic!("expected the resume wizard, got {:?}", dashboard.mode);
    };
    assert_eq!(wizard.source, crate::wizards::ResumeSource::Archive);
    assert_eq!(wizard.session_id, "gone");
}

/// A configuration with a bare target beside the container one, so the two
/// halves of the target-naming rule can both be seen.
fn config_with_bare_target() -> Config {
    let mut config = config();
    config.targets.insert(
        "localhost".into(),
        mj_core::config::TargetTemplate::LocalBare,
    );
    config
}

/// A Mjolnir session the index carries metadata for: the sync stored its
/// target, profile and harness as tags, and the row is built from them.
fn tagged_wiki_row(id: &str, target: &str, profile: &str) -> WikiRow {
    WikiRow {
        target: Some(target.into()),
        profile: Some(profile.into()),
        harness: Some("codex".into()),
        ..wiki_row(id, true)
    }
}

/// An archived row names the profile and target the index carries, instead of
/// guessing `mjolnir` and a local origin. A bare target is named with the
/// project it opened; a container target names itself; a target the
/// configuration no longer holds is shown verbatim.
#[test]
fn archived_row_shows_indexed_target_and_profile() {
    let config = config_with_bare_target();
    let state = state_with(Vec::new());

    let archived = |hit: WikiRow| {
        merged_resume_rows(&config, &state, &[], &[hit])
            .into_iter()
            .find(|row| matches!(row.key, ResumeRowKey::Archive(_)))
            .expect("the indexed session becomes an archived row")
    };

    let bare = archived(tagged_wiki_row("bare", "localhost", "codex-2"));
    assert_eq!(
        bare.profile_id, "codex-2",
        "the indexed profile, not the tool"
    );
    assert_eq!(bare.origin, "localhost/project");

    // C-14: a session that ran in a managed worktree is named by the project
    // it worked on, not by the worktree's session-id directory.
    let worktree = archived(WikiRow {
        project: "/home/dev/project/.mj/worktrees/556ebcbaee181".into(),
        ..tagged_wiki_row("worktree", "localhost", "codex-2")
    });
    assert_eq!(worktree.origin, "localhost/project");
    let nested = archived(WikiRow {
        project: "/home/dev/project/.mj/worktrees/556ebcbaee181/sub".into(),
        target: None,
        ..tagged_wiki_row("nested", "localhost", "codex-2")
    });
    assert_eq!(nested.origin, "local/sub");

    let container = archived(tagged_wiki_row("container", "podman", "codex-2"));
    assert_eq!(container.origin, "podman");

    let retired = archived(tagged_wiki_row("retired", "was-a-target", "codex-2"));
    assert_eq!(
        retired.origin, "was-a-target",
        "a target the configuration lost is shown as it was recorded"
    );

    // A row the index carries nothing for keeps what it always showed.
    let untagged = archived(wiki_row("untagged", true));
    assert_eq!(untagged.profile_id, "mjolnir");
    assert_eq!(untagged.origin, "local/project");
}

/// Put the dialog on the Archived tab with its one row selected, which is
/// where the arrow keys leave it. The test that follows is about what Enter
/// does, not about reaching the tab.
fn select_the_archived_row(dashboard: &mut DashboardState) {
    dashboard.switch_resume_tab(ResumeTab::Archive);
    if let Mode::ResumeDialog(dialog) = &mut dashboard.mode {
        dialog.form.get_mut().focus(ResumeFocus::Sessions);
    }
    dashboard.select_resume_row(0);
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert_eq!(
        dialog.selected,
        Some(ResumeRowKey::Archive("gone".into())),
        "the archived row is selected"
    );
}

/// Enter on an archived row opens the restore wizard already pointed at the
/// profile and target the session ran under.
#[test]
fn enter_on_archived_row_preseeds_wizard_profile_and_target() {
    let mut dashboard = DashboardState::new(
        config_with_bare_target(),
        state_with(Vec::new()),
        BTreeMap::new(),
    );
    dashboard.show_resume_dialog(1, vec![codex_profile(Vec::new())]);
    let (request_id, _) = dashboard.next_wiki_search().expect("a search is asked for");
    dashboard.apply_wiki_search(
        request_id,
        ready_page(vec![tagged_wiki_row("gone", "podman", "codex-2")]),
    );

    select_the_archived_row(&mut dashboard);
    dashboard.handle_key(key(KeyCode::Enter));

    let Mode::Resume(wizard) = &dashboard.mode else {
        panic!("expected the resume wizard, got {:?}", dashboard.mode);
    };
    // `config_with_bare_target` lists claude-1, codex-1, codex-2 in that
    // order, and localhost, podman in that order.
    assert_eq!(wizard.profile, 2, "the wizard opens on codex-2");
    assert_eq!(wizard.target, 1, "the wizard opens on podman");
}

/// An id the configuration no longer has leaves the wizard on its first
/// choice; there is nothing better to offer.
#[test]
fn an_unknown_indexed_profile_leaves_the_restore_wizard_on_its_first_choice() {
    let mut dashboard = DashboardState::new(
        config_with_bare_target(),
        state_with(Vec::new()),
        BTreeMap::new(),
    );
    dashboard.show_resume_dialog(1, vec![codex_profile(Vec::new())]);
    let (request_id, _) = dashboard.next_wiki_search().expect("a search is asked for");
    dashboard.apply_wiki_search(
        request_id,
        ready_page(vec![tagged_wiki_row(
            "gone",
            "was-a-target",
            "was-a-profile",
        )]),
    );
    select_the_archived_row(&mut dashboard);
    dashboard.handle_key(key(KeyCode::Enter));

    let Mode::Resume(wizard) = &dashboard.mode else {
        panic!("expected the resume wizard, got {:?}", dashboard.mode);
    };
    assert_eq!(wizard.profile, 0);
    assert_eq!(wizard.target, 0);
}

/// Typing asks the daemon for a new search, and an answer to an older
/// request is ignored because the person has typed since.
#[test]
fn typing_asks_for_a_search_and_stale_answers_are_dropped() {
    let mut dashboard = DashboardState::new(
        config(),
        state_with(vec![stopped_session()]),
        BTreeMap::new(),
    );
    dashboard.show_resume_dialog(1, vec![codex_profile(Vec::new())]);
    apply_ready_rows(&mut dashboard, Vec::new());
    // The Live tab matches names itself; the index is asked from the tabs that
    // have nothing else to search with.
    switch_to_hel(&mut dashboard);
    focus_resume_control(&mut dashboard, ResumeFocus::Search);

    let action = dashboard.handle_key(key(KeyCode::Char('g')));
    let DashboardAction::SearchArchivedSessions { request_id, query } = action else {
        panic!("typing must ask for a search, got {action:?}");
    };
    assert_eq!(query, "g");

    dashboard.apply_wiki_search(request_id - 1, ready_page(vec![wiki_row("stale", true)]));
    assert!(
        !rows(&dashboard)
            .iter()
            .any(|row| matches!(row.key, ResumeRowKey::Archive(_))),
        "an answer to an older request is dropped"
    );
    dashboard.apply_wiki_search(request_id, ready_page(vec![wiki_row("fresh", true)]));
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert_eq!(dialog.wiki.len(), 1);
    assert_eq!(dialog.wiki[0].id, "fresh");
}

#[test]
fn tabs_separate_hel_records_from_importable_native_sessions() {
    let mut dashboard = DashboardState::new(
        config(),
        state_with(vec![stopped_session()]),
        BTreeMap::new(),
    );
    dashboard.show_resume_dialog(
        1,
        vec![codex_profile(vec![native(
            "native-2",
            "Native",
            NEWER_THAN_THE_CHECKPOINT,
        )])],
    );
    switch_to_hel(&mut dashboard);

    assert_eq!(titles(&rows(&dashboard)), ["ACP pretty name"]);
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert_eq!(dialog.tab, ResumeTab::Hel);
    assert_eq!(dialog.selected, Some(ResumeRowKey::Hel("session-1".into())));

    switch_to_import(&mut dashboard);
    assert_eq!(titles(&rows(&dashboard)), ["Native"]);
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert_eq!(
        dialog.selected,
        Some(ResumeRowKey::Native(HarnessKind::Codex, "native-2".into()))
    );

    dashboard.handle_key(key(KeyCode::Left));
    assert_eq!(titles(&rows(&dashboard)), ["ACP pretty name"]);
}

#[test]
fn search_arrows_edit_the_cursor_and_tabs_have_their_own_focus() {
    let mut dashboard = DashboardState::new(
        config(),
        state_with(vec![stopped_session()]),
        BTreeMap::new(),
    );
    dashboard.show_resume_dialog(
        1,
        vec![codex_profile(vec![native(
            "native-2",
            "Native",
            NEWER_THAN_THE_CHECKPOINT,
        )])],
    );
    apply_ready_rows(&mut dashboard, Vec::new());
    switch_to_hel(&mut dashboard);
    // Switching tabs leaves the box for the list; `/` is the way back to it.
    dashboard.handle_key(key(KeyCode::Char('/')));
    dashboard.handle_paste("nat");
    dashboard.handle_key(key(KeyCode::Left));
    dashboard.handle_key(key(KeyCode::Char('X')));
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("resume")
    };
    assert_eq!(dialog.tab, ResumeTab::Hel);
    assert_eq!(dialog.search.value(), "naXt");
    assert_eq!(dialog.focused(), ResumeFocus::Search);
    dashboard.handle_key(key(KeyCode::BackTab));
    dashboard.handle_key(key(KeyCode::Right));
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("resume")
    };
    assert_eq!(dialog.tab, ResumeTab::Import);
    assert_eq!(dialog.focused(), ResumeFocus::Tabs);
}

/// Selecting a row dispatches to the flow that suits its source: the
/// resume wizard for a Hel record, the import flow for a native session.
#[test]
fn selecting_a_row_resumes_a_hel_record_and_imports_a_native_session() {
    let mut dashboard = DashboardState::new(
        config(),
        state_with(vec![stopped_session()]),
        BTreeMap::new(),
    );
    dashboard.show_resume_dialog(
        1,
        vec![codex_profile(vec![native(
            "native-2",
            "Native",
            NEWER_THAN_THE_CHECKPOINT,
        )])],
    );
    switch_to_hel(&mut dashboard);

    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert!(matches!(dashboard.mode, Mode::Resume(_)));

    dashboard.show_resume_dialog(
        2,
        vec![codex_profile(vec![native(
            "native-2",
            "Native",
            NEWER_THAN_THE_CHECKPOINT,
        )])],
    );
    switch_to_import(&mut dashboard);
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::ImportSession {
            profile_id: "codex-1".into(),
            native_session_id: "native-2".into(),
            display_title: "Native".into(),
        }
    );
}

/// Enter on the focused Mjolnir row starts its resume, which is what the tab's
/// own hint promises and what the Resume button already does.
#[test]
fn enter_on_a_focused_mjolnir_row_begins_its_resume() {
    let mut dashboard = DashboardState::new(
        config(),
        state_with(vec![stopped_session()]),
        BTreeMap::new(),
    );
    dashboard.show_resume_dialog(1, Vec::new());
    let mut hit = wiki_row("wiki-1", false);
    hit.hel_session_id = Some("session-1".into());
    apply_ready_rows(&mut dashboard, vec![hit]);
    // Drawing registers the dialog's controls, so the keys land where they do in
    // the running dashboard. Live is empty here: the only session is stopped.
    drawn(&mut dashboard, 120, 40);
    assert!(rows(&dashboard).is_empty(), "no session is running");

    dashboard.handle_key(key(KeyCode::Right));
    drawn(&mut dashboard, 120, 40);
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert_eq!(dialog.tab, ResumeTab::Hel);
    assert_eq!(rows(&dashboard).len(), 1, "the stopped session is listed");

    assert_eq!(dialog_focus(&dashboard), ResumeFocus::Sessions);
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert!(
        matches!(dashboard.mode, Mode::Resume(_)),
        "Enter on the row opens the resume wizard"
    );
}

/// Two running sessions in the active workspace and one in `other`, each with
/// its own title so a name search can pick one out.
fn dashboard_with_live_sessions_in_two_workspaces() -> DashboardState {
    let mut sessions = Vec::new();
    for (id, workspace, title) in [
        ("live-alpha", "default", "Sweep the hearth"),
        ("live-beta", "default", "Count the coins"),
        ("live-remote", "other", "Raise the mast"),
    ] {
        let mut session = running_session();
        session.id = id.into();
        session.workspace_id = workspace.into();
        session.session_title_override = Some(title.into());
        session.native_session_id = None;
        sessions.push(session);
    }
    let mut dashboard = DashboardState::new(config(), state_with(sessions), BTreeMap::new());
    dashboard.set_workspace_names(BTreeMap::from([
        ("default".into(), "Default".into()),
        ("other".into(), "Other".into()),
    ]));
    dashboard
}

/// The Live tab lists the running sessions of every workspace, and its search
/// box narrows them by name while the index is still building.
#[test]
fn the_live_tab_lists_running_sessions_everywhere_and_searches_them_by_name() {
    let mut dashboard = dashboard_with_live_sessions_in_two_workspaces();
    dashboard.show_resume_dialog(1, Vec::new());
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert_eq!(dialog.tab, ResumeTab::Live, "the dialog opens on Live");

    let listed = rows(&dashboard)
        .into_iter()
        .map(|row| match row.key {
            ResumeRowKey::Live(session_id) => session_id,
            other => panic!("the Live tab listed {other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(listed.len(), 3, "every running session is listed");
    assert_eq!(
        listed
            .iter()
            .map(|session_id| dashboard.state.sessions[session_id].workspace_id.clone())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["default".to_owned(), "other".to_owned()]),
        "the list spans both workspaces"
    );

    // Live is the leftmost tab, so from the list Left wraps around to the last
    // one rather than landing in the middle of the strip.
    focus_resume_control(&mut dashboard, ResumeFocus::Sessions);
    dashboard.handle_key(key(KeyCode::Left));
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert_eq!(dialog.tab, ResumeTab::Archive);
    switch_to_tab(&mut dashboard, ResumeTab::Live);

    // Drawing registers the dialog's controls, so `/` reaches the box the way
    // it does in the running dashboard, with the index still building.
    drawn(&mut dashboard, 120, 40);
    dashboard.handle_key(key(KeyCode::Char('/')));
    focus_resume_control(&mut dashboard, ResumeFocus::Search);
    for character in "mast".chars() {
        assert!(
            matches!(
                dashboard.handle_key(key(KeyCode::Char(character))),
                DashboardAction::SearchArchivedSessions { .. }
            ),
            "the Live list answers immediately while the index searches the other tabs"
        );
    }
    assert_eq!(titles(&rows(&dashboard)), ["Raise the mast"]);
}

/// I1-16: LAST ACTIVE for a running session is its last activity, not the
/// time its record was last written (often its creation).
#[test]
fn the_live_tab_shows_each_sessions_last_activity() {
    let mut dashboard = dashboard_with_live_sessions_in_two_workspaces();
    let session_id = dashboard
        .state
        .sessions
        .keys()
        .next()
        .cloned()
        .expect("a live session");
    let recorded = timestamp_ms(&dashboard.state.sessions[&session_id].updated_at)
        .expect("the record has a time");
    let active = recorded + 20 * 60 * 1000;
    dashboard.session_details.insert(
        session_id.clone(),
        crate::ingest::SessionDetail {
            last_activity_at_ms: Some(u64::try_from(active).unwrap()),
            ..crate::ingest::SessionDetail::default()
        },
    );
    dashboard.show_resume_dialog(1, Vec::new());
    let row = rows(&dashboard)
        .into_iter()
        .find(|row| row.key == ResumeRowKey::Live(session_id.clone()))
        .expect("the session is listed");
    assert_eq!(row.last_activity_ms, active);
}

/// Enter on a running session in another workspace closes the dialog and takes
/// the dashboard there, the same jump the attention key makes.
#[test]
fn enter_on_a_live_row_moves_the_dashboard_to_that_session() {
    let mut dashboard = dashboard_with_live_sessions_in_two_workspaces();
    dashboard.show_resume_dialog(1, Vec::new());
    let index = rows(&dashboard)
        .iter()
        .position(|row| row.key == ResumeRowKey::Live("live-remote".into()))
        .expect("the other workspace's session is listed");
    dashboard.select_resume_row(index);
    // The dialog opens with the list itself focused, so Enter already acts on it.
    assert_eq!(dialog_focus(&dashboard), ResumeFocus::Sessions);

    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::SelectWorkspace {
            workspace_id: "other".into()
        }
    );
    assert!(
        matches!(dashboard.mode, Mode::Dashboard),
        "the dialog closed"
    );
    dashboard.set_active_workspace(Some("other".into()));
    assert_eq!(dashboard.selected_session_id(), Some("live-remote"));
}

/// The same three live sessions, each at a different attention level, so a
/// state filter has something to keep and something to drop.
fn dashboard_with_live_attention_mix() -> DashboardState {
    let mut dashboard = dashboard_with_live_sessions_in_two_workspaces();
    dashboard
        .session_details
        .get_mut("live-remote")
        .expect("live-remote's details")
        .pending_elicitations = vec![question("live-remote")];
    dashboard
        .session_details
        .get_mut("live-beta")
        .expect("live-beta's details")
        .unread_agent_messages = 1;
    dashboard
}

/// The dialog opens on the running sessions with the list itself focused, and
/// the letters the Sessions pane uses narrow it by state immediately.
#[test]
fn the_dialog_opens_on_live_sessions_and_the_state_letters_narrow_them() {
    let mut dashboard = dashboard_with_live_attention_mix();
    dashboard.show_resume_dialog(1, Vec::new());
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert_eq!(dialog.tab, ResumeTab::Live, "the dialog opens on Live");
    assert_eq!(
        dialog.focused(),
        ResumeFocus::Sessions,
        "the list has focus from the moment the dialog opens"
    );
    assert_eq!(rows(&dashboard).len(), 3);

    drawn(&mut dashboard, 120, 40);
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Char('b'))),
        DashboardAction::None
    );
    assert_eq!(
        rows(&dashboard)
            .into_iter()
            .map(|row| row.key)
            .collect::<Vec<_>>(),
        vec![ResumeRowKey::Live("live-remote".into())],
        "only the session waiting on a question counts as blocked"
    );
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Char('d'))),
        DashboardAction::None
    );
    assert_eq!(
        rows(&dashboard)
            .into_iter()
            .map(|row| row.key)
            .collect::<Vec<_>>(),
        vec![ResumeRowKey::Live("live-beta".into())],
        "the unread answer is the done one"
    );
    assert_eq!(
        dashboard.selected_session_id(),
        Some("live-alpha"),
        "the letters move the dialog's own cursor, not the conversation the pane has open"
    );
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Char('a'))),
        DashboardAction::None
    );
    assert_eq!(rows(&dashboard).len(), 3, "`a` brings the whole list back");
}

/// The Live tab matches names immediately and asks the index so the other
/// tabs' counts can update before the person leaves Live.
#[test]
fn a_live_tab_search_updates_other_tab_counts_before_switching() {
    let mut dashboard = dashboard_with_live_sessions_in_two_workspaces();
    dashboard.show_resume_dialog(1, Vec::new());
    // Drawing registers the dialog's controls, so `/` reaches the box; the
    // dialog itself opens with the list focused, not the box.
    drawn(&mut dashboard, 120, 40);
    dashboard.handle_key(key(KeyCode::Char('/')));
    for character in "mast".chars() {
        assert!(matches!(
            dashboard.handle_key(key(KeyCode::Char(character))),
            DashboardAction::SearchArchivedSessions { .. }
        ));
    }
    assert_eq!(titles(&rows(&dashboard)), ["Raise the mast"]);

    apply_ready_rows(&mut dashboard, vec![wiki_row("gone", true)]);
    assert_eq!(dashboard.resume_hit_counts, [0, 0, 0, 1]);
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert_eq!(
        resume_tab_labels(&dashboard, dialog),
        [" Live ", " Mjolnir · 0 ", " Import · 0 ", " Archived · 1 "]
    );

    // Switching tabs uses the answer already requested while typing.
    let action = dashboard.handle_key(key(KeyCode::Right));
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert_eq!(dialog.tab, ResumeTab::Hel);
    assert!(!matches!(
        action,
        DashboardAction::SearchArchivedSessions { .. }
    ));
}

/// The arrows go on walking the strip after landing on a tab with nothing in
/// it, which is every history tab while all of a dashboard's sessions run.
#[test]
fn the_arrows_walk_the_strip_across_a_tab_whose_list_is_empty() {
    let mut dashboard = dashboard_with_live_sessions_in_two_workspaces();
    dashboard.show_resume_dialog(1, Vec::new());
    drawn(&mut dashboard, 120, 40);
    assert_eq!(dialog_focus(&dashboard), ResumeFocus::Sessions);

    dashboard.handle_key(key(KeyCode::Right));
    drawn(&mut dashboard, 120, 40);
    assert!(rows(&dashboard).is_empty(), "nothing is stopped");
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert_eq!(dialog.tab, ResumeTab::Hel);
    assert_eq!(
        dialog.focused(),
        ResumeFocus::Tabs,
        "an empty list cannot hold the keyboard, so the strip keeps it"
    );

    dashboard.handle_key(key(KeyCode::Right));
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert_eq!(dialog.tab, ResumeTab::Import, "the arrows are still live");

    dashboard.handle_key(key(KeyCode::Left));
    dashboard.handle_key(key(KeyCode::Left));
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert_eq!(
        dialog.tab,
        ResumeTab::Live,
        "and Left comes all the way back"
    );
    assert_eq!(
        dialog.focused(),
        ResumeFocus::Sessions,
        "a tab with rows hands the keyboard back to them"
    );
}

/// In the search box the arrows belong to the caret, as readline has them, and
/// they reach the tab strip only from the end they are pressed against.
#[test]
fn the_search_arrows_reach_the_strip_only_from_the_end_of_the_query() {
    let mut dashboard = dashboard_with_live_sessions_in_two_workspaces();
    dashboard.show_resume_dialog(1, Vec::new());
    apply_ready_rows(&mut dashboard, Vec::new());
    drawn(&mut dashboard, 120, 40);
    dashboard.handle_key(key(KeyCode::Char('/')));
    dashboard.handle_paste("mast");

    dashboard.handle_key(key(KeyCode::Left));
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert_eq!(dialog.search.cursor(), 3, "the caret moved, not the tab");
    assert_eq!(dialog.tab, ResumeTab::Live);

    dashboard.handle_key(key(KeyCode::Right));
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert_eq!(dialog.search.cursor(), 4, "the caret reached the end");
    assert_eq!(dialog.tab, ResumeTab::Live);

    dashboard.handle_key(key(KeyCode::Right));
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert_eq!(
        dialog.tab,
        ResumeTab::Hel,
        "pressed against the end it tabs"
    );
    assert_eq!(dialog.search.value(), "mast", "the query comes along");
    assert_eq!(
        dialog.focused(),
        ResumeFocus::Search,
        "a person still typing keeps the box"
    );

    dashboard.handle_key(key(KeyCode::Left));
    dashboard.handle_key(key(KeyCode::Left));
    dashboard.handle_key(key(KeyCode::Left));
    dashboard.handle_key(key(KeyCode::Left));
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert_eq!(dialog.search.cursor(), 0);
    assert_eq!(dialog.tab, ResumeTab::Hel, "four Lefts only walk the query");

    dashboard.handle_key(key(KeyCode::Left));
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert_eq!(dialog.tab, ResumeTab::Live, "the fifth reaches the strip");
}

/// Escape takes the query before it takes the dialog, then hands the keyboard
/// back to the list, matching the help overlay and the Sessions pane's filter.
#[test]
fn escape_clears_the_query_then_leaves_the_box_then_closes_the_dialog() {
    let mut dashboard = dashboard_with_live_sessions_in_two_workspaces();
    dashboard.show_resume_dialog(1, Vec::new());
    drawn(&mut dashboard, 120, 40);
    dashboard.handle_key(key(KeyCode::Char('/')));
    dashboard.handle_paste("mast");
    assert_eq!(titles(&rows(&dashboard)), ["Raise the mast"]);

    dashboard.handle_key(key(KeyCode::Esc));
    assert_eq!(rows(&dashboard).len(), 3, "the whole list is back");
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("the dialog stays open while there is a query to clear");
    };
    assert!(dialog.search.is_empty(), "the query went, not the dialog");
    assert_eq!(
        dialog.focused(),
        ResumeFocus::Search,
        "clearing the box does not move the keyboard out of it"
    );

    dashboard.handle_key(key(KeyCode::Esc));
    assert_eq!(
        dialog_focus(&dashboard),
        ResumeFocus::Sessions,
        "an empty box gives the list back"
    );

    dashboard.handle_key(key(KeyCode::Esc));
    assert!(
        matches!(dashboard.mode, Mode::Dashboard),
        "with nothing left to peel, Escape closes the dialog"
    );
}

/// Escape still reaches the dialog from a tab with an empty list, where there
/// is no row for the box's focus to be handed to.
#[test]
fn escape_closes_the_dialog_from_a_tab_with_nothing_in_it() {
    let mut dashboard = dashboard_with_live_sessions_in_two_workspaces();
    dashboard.show_resume_dialog(1, Vec::new());
    apply_ready_rows(&mut dashboard, Vec::new());
    drawn(&mut dashboard, 120, 40);
    dashboard.handle_key(key(KeyCode::Right));
    drawn(&mut dashboard, 120, 40);
    assert!(rows(&dashboard).is_empty(), "nothing is stopped");
    dashboard.handle_key(key(KeyCode::Char('/')));
    assert_eq!(dialog_focus(&dashboard), ResumeFocus::Search);

    dashboard.handle_key(key(KeyCode::Esc));
    assert_eq!(
        dialog_focus(&dashboard),
        ResumeFocus::Tabs,
        "with no row to take it, the strip takes the keyboard"
    );
    dashboard.handle_key(key(KeyCode::Esc));
    assert!(matches!(dashboard.mode, Mode::Dashboard));
}

/// One match is not "1 matches". The heading counts in the reader's grammar.
#[test]
fn the_search_heading_counts_a_single_match_in_the_singular() {
    let mut dashboard = dashboard_with_live_sessions_in_two_workspaces();
    dashboard.show_resume_dialog(1, Vec::new());
    // A still-building index says so in the heading instead of counting.
    apply_ready_rows(&mut dashboard, Vec::new());
    replace_search(&mut dashboard, "mast");
    let lines = drawn(&mut dashboard, 120, 40).join("\n");
    assert!(lines.contains(" 1 match "), "{lines}");
    assert!(!lines.contains("1 matches"), "{lines}");

    // The plural survives; only the count of one was wrong.
    replace_search(&mut dashboard, "Default");
    let lines = drawn(&mut dashboard, 120, 40).join("\n");
    assert!(lines.contains(" 2 matches "), "{lines}");
}

/// The dashboard lists live sessions; the dialog lists the rest. Nothing
/// appears in both, and a stop in progress stays on the dashboard until
/// the state machine reaches Stopped.
#[test]
fn the_sidebar_shows_live_work_and_resume_lists_settled_history() {
    let mut sessions = Vec::new();
    for (index, state) in [
        SessionState::Provisioning,
        SessionState::Running,
        SessionState::Disconnected,
        SessionState::Checkpointing,
        SessionState::Closing,
        SessionState::Destroying,
        SessionState::Error,
        SessionState::Stopped,
        SessionState::Lost,
        SessionState::DestroyedWithDataLoss,
    ]
    .into_iter()
    .enumerate()
    {
        let mut session = stopped_session();
        session.id = format!("session-{index:02}");
        session.native_session_id = None;
        session.state = state;
        sessions.push(session);
    }
    let mut dashboard = DashboardState::new(config(), state_with(sessions), BTreeMap::new());
    dashboard.show_resume_dialog(1, Vec::new());
    switch_to_hel(&mut dashboard);

    let on_dashboard = dashboard
        .ordered_sessions()
        .iter()
        .map(|session| session.state)
        .collect::<Vec<_>>();
    assert_eq!(on_dashboard.len(), 7);
    assert!(!on_dashboard.contains(&SessionState::Stopped));
    // Closing and Checkpointing are mid-stop and must not vanish.
    assert!(on_dashboard.contains(&SessionState::Closing));
    assert!(on_dashboard.contains(&SessionState::Checkpointing));

    let in_dialog = rows(&dashboard)
        .into_iter()
        .map(|row| row.key)
        .collect::<Vec<_>>();
    assert_eq!(in_dialog.len(), 3);
    let dashboard_ids = dashboard
        .ordered_sessions()
        .iter()
        .map(|session| ResumeRowKey::Hel(session.id.clone()))
        .collect::<Vec<_>>();
    assert!(
        in_dialog.iter().all(|key| !dashboard_ids.contains(key)),
        "settled history stays in Resume rather than the active sidebar"
    );
}

/// Scans arrive one profile at a time; folding one in must not move the
/// Import-tab selection off the native row the user was on.
#[test]
fn an_incremental_scan_update_keeps_the_selected_row() {
    let mut dashboard = DashboardState::new(
        config(),
        state_with(vec![stopped_session()]),
        BTreeMap::new(),
    );
    dashboard.show_resume_dialog(1, vec![codex_profile(vec![native("native-2", "Older", 1)])]);
    switch_to_import(&mut dashboard);
    assert_eq!(titles(&rows(&dashboard)), ["Older"]);
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert_eq!(dialog.row_index, 0);

    // A newer native session arrives and sorts above the selected row.
    dashboard.apply_resume_profile(
        1,
        codex_profile(vec![
            native("native-2", "Older", 1),
            native("native-3", "Newer", NEWER_THAN_THE_CHECKPOINT),
        ]),
    );
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert_eq!(
        dialog.selected,
        Some(ResumeRowKey::Native(HarnessKind::Codex, "native-2".into()))
    );
    assert_eq!(dialog.row_index, 1);
    // A late update for another discovery is ignored.
    dashboard.apply_resume_profile(99, codex_profile(Vec::new()));
    assert_eq!(rows(&dashboard).len(), 2);
}

#[test]
fn provider_archive_metadata_does_not_move_the_selected_row() {
    let mut dashboard = DashboardState::new(config(), state_with(Vec::new()), BTreeMap::new());
    dashboard.show_resume_dialog(
        1,
        vec![codex_profile(vec![
            native("native-new", "Newer", NEWER_THAN_THE_CHECKPOINT),
            native("native-old", "Older", 1),
        ])],
    );
    switch_to_import(&mut dashboard);
    dashboard.handle_key(key(KeyCode::Down));
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert_eq!(dialog.row_index, 1);
    assert_eq!(
        dialog.selected,
        Some(ResumeRowKey::Native(
            HarnessKind::Codex,
            "native-old".into()
        ))
    );

    let mut old = native("native-old", "Older", 1);
    old.natively_archived = true;
    dashboard.apply_resume_profile(
        1,
        codex_profile(vec![
            native("native-new", "Newer", NEWER_THAN_THE_CHECKPOINT),
            old,
        ]),
    );

    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert_eq!(dialog.row_index, 1);
    assert_eq!(
        dialog.selected,
        Some(ResumeRowKey::Native(
            HarnessKind::Codex,
            "native-old".into()
        ))
    );
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::ImportSession {
            profile_id: "codex-1".into(),
            native_session_id: "native-old".into(),
            display_title: "Older".into(),
        }
    );
    assert!(matches!(dashboard.mode, Mode::Dashboard));
}

/// A scan that failed is reported rather than dropped.
#[test]
fn a_failed_profile_scan_is_reported_in_the_dialog() {
    let mut dashboard = DashboardState::new(config(), state_with(Vec::new()), BTreeMap::new());
    dashboard.show_resume_dialog(
        1,
        vec![ImportProfileOption {
            profile_id: "codex-1".into(),
            harness_kind: HarnessKind::Codex,
            sessions: Vec::new(),
            scan_progress: None,
            error: Some("permission denied".into()),
        }],
    );
    switch_to_import(&mut dashboard);
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .expect("draw the resume dialog");
    let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
    assert!(rendered.contains("Scan failed for codex-1"), "{rendered}");
}

#[test]
fn resume_table_has_headers_repeated_profiles_and_last_active_values() {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut dashboard = DashboardState::new(config(), state_with(Vec::new()), BTreeMap::new());
    dashboard.show_resume_dialog(
        1,
        vec![codex_profile(vec![
            native("native-1", "Recent session", now_ms - 2 * 60_000),
            native("native-2", "Older session", now_ms - 60 * 60_000),
        ])],
    );
    switch_to_import(&mut dashboard);

    let mut terminal = Terminal::new(TestBackend::new(140, 34)).expect("terminal");
    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .expect("draw the resume dialog");
    let rendered = buffer_lines(terminal.backend().buffer()).join("\n");

    for heading in ["PROFILE", "TARGET", "LAST ACTIVE", "SESSION"] {
        assert!(rendered.contains(heading), "{rendered}");
    }
    for title in ["Recent session", "Older session"] {
        let row = rendered
            .lines()
            .find(|line| line.contains(title))
            .expect("rendered session row");
        assert!(row.contains("codex-1"), "{row}");
    }
    assert!(rendered.contains("2 minutes ago"), "{rendered}");
    assert!(rendered.contains("1 hour ago"), "{rendered}");
    assert!(rendered.contains("Search:"), "{rendered}");
}

/// The Live tab spends the profile and target columns' width on a workspace
/// column and a wider title instead; the other tabs are unaffected. Dropping
/// the profile cell must drop its separator too, or the workspace column
/// opens two cells later than it should.
#[test]
fn the_live_tab_shows_a_workspace_column_instead_of_profile_and_target() {
    let mut dashboard = dashboard_with_live_sessions_in_two_workspaces();
    // A stopped session gives the Hel tab a row too, so its row content's
    // starting column is a reference point for the row-level gap check below.
    let hel_session = stopped_session();
    dashboard
        .state
        .sessions
        .insert(hel_session.id.clone(), hel_session);
    dashboard.show_resume_dialog(1, Vec::new());
    switch_to_tab(&mut dashboard, ResumeTab::Live);

    let live_lines = drawn(&mut dashboard, 140, 34);
    let header = live_lines
        .iter()
        .find(|line| line.contains("WORKSPACE"))
        .expect("the Live tab's header names the workspace column");
    assert!(!header.contains("PROFILE"), "{header}");
    assert!(!header.contains("TARGET"), "{header}");
    let (workspace_header_column, header_row) = point(&live_lines, "WORKSPACE");

    // Search below the header row so this does not match the dashboard's own
    // workspace tab strip, drawn behind the modal and also naming "Default".
    let row = live_lines[usize::from(header_row) + 1..]
        .iter()
        .find(|line| line.contains("Default"))
        .expect("a Live row names its workspace");
    let workspace_row_column = cell_column(row, "Default");

    // The Resume (Hel) tab keeps both columns. Its header and its row content
    // start at the same columns the Live tab's do; if the Live layout still
    // reserved the profile cell's separator, the Live header and row would
    // each start two cells later than their Hel-tab counterparts.
    switch_to_tab(&mut dashboard, ResumeTab::Hel);
    let hel_lines = drawn(&mut dashboard, 140, 34);
    let header = hel_lines
        .iter()
        .find(|line| line.contains("PROFILE"))
        .expect("the Resume tab's header keeps PROFILE");
    assert!(header.contains("TARGET"), "{header}");
    let (profile_header_column, header_row) = point(&hel_lines, "PROFILE");
    assert_eq!(
        profile_header_column, workspace_header_column,
        "no stray gap should open in the header where the profile cell used to be"
    );

    let row = hel_lines[usize::from(header_row) + 1..]
        .iter()
        .find(|line| line.contains("codex-1"))
        .expect("the Hel tab lists the stopped session's profile");
    let profile_row_column = cell_column(row, "codex-1");
    assert_eq!(
        profile_row_column, workspace_row_column,
        "no stray gap should open in Live rows where the profile cell used to be"
    );
}

/// The dialog is the only surface for non-live sessions, and the resume chord
/// opens it from anywhere.
#[test]
fn the_dashboard_opens_the_dialog_and_names_the_key_in_the_footer() {
    let mut dashboard = dashboard_with_session(running_session());
    assert_eq!(
        chord(&mut dashboard, crate::CommandId::ResumeDialog),
        DashboardAction::OpenResumeDialog
    );
    assert_eq!(dashboard.handle_key(ctrl_key('t')), DashboardAction::None);
    assert_eq!(dashboard.focus, Focus::Sessions);

    let mut terminal = Terminal::new(TestBackend::new(140, 30)).expect("terminal");
    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .expect("draw the dashboard");
    let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
    assert!(rendered.contains("g sessions"), "{rendered}");
    assert!(!rendered.contains("Import"), "{rendered}");
}

/// The rows are derived state, rebuilt where their inputs change. A state
/// reload and a checkpoint size that arrives from the background reach the
/// open dialog straight away.
#[test]
fn the_row_list_follows_state_reloads_and_background_updates_while_the_dialog_is_open() {
    let mut dashboard = DashboardState::new(
        config(),
        state_with(vec![stopped_session()]),
        BTreeMap::new(),
    );
    dashboard.show_resume_dialog(
        1,
        vec![codex_profile(vec![native(
            "native-2",
            "Native",
            NEWER_THAN_THE_CHECKPOINT,
        )])],
    );
    switch_to_hel(&mut dashboard);
    assert_eq!(titles(&rows(&dashboard)), ["ACP pretty name"]);

    let mut reloaded = stopped_session();
    reloaded.id = "session-2".into();
    reloaded.native_session_id = None;
    reloaded.acp_session_title = Some("Reloaded record".into());
    dashboard.set_state(state_with(vec![stopped_session(), reloaded]));
    assert!(
        titles(&rows(&dashboard)).contains(&"Reloaded record"),
        "{:?}",
        titles(&rows(&dashboard))
    );

    dashboard
        .apply_checkpoint_archive_sizes(BTreeMap::from([("session-1".to_owned(), Some(2_048))]));
    let listed = rows(&dashboard);
    let sized = listed
        .iter()
        .find(|row| row.key == ResumeRowKey::Hel("session-1".into()))
        .expect("the checkpointed record");
    assert!(sized.details.contains("2.0K"), "{}", sized.details);

    switch_to_import(&mut dashboard);
    assert_eq!(titles(&rows(&dashboard)), ["Native"]);
    assert_eq!(titles(&rows(&dashboard)), ["Native"]);
}

/// Walking the list moves the selection over rows that stay put: an arrow
/// key changes nothing the rows are built from.
#[test]
fn arrow_navigation_moves_the_selection_without_changing_the_row_list() {
    let mut dashboard = DashboardState::new(
        config(),
        state_with(vec![stopped_session()]),
        BTreeMap::new(),
    );
    dashboard.show_resume_dialog(
        1,
        vec![codex_profile(vec![
            native("native-2", "Native newer", NEWER_THAN_THE_CHECKPOINT),
            native("native-3", "Native older", 1),
        ])],
    );
    switch_to_import(&mut dashboard);
    let before = rows(&dashboard);
    assert_eq!(before.len(), 2, "{:?}", titles(&before));

    dashboard.handle_key(key(KeyCode::Down));

    assert_eq!(rows(&dashboard), before, "navigation rebuilt the rows");
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert_eq!(dialog.row_index, 1);
    assert_eq!(dialog.selected, Some(before[1].key.clone()));
}

/// One search path: a query lists the rows the index returned and nothing
/// else, on every tab, in the order the index ranked them.
#[test]
fn a_query_lists_only_what_the_index_returned_in_its_own_order() {
    let mut dashboard = DashboardState::new(
        config(),
        state_with(vec![stopped_session()]),
        BTreeMap::new(),
    );
    dashboard.show_resume_dialog(
        1,
        vec![codex_profile(vec![
            native("native-2", "Native alpha", NEWER_THAN_THE_CHECKPOINT),
            native("native-3", "Native beta", 1),
        ])],
    );
    apply_ready_rows(&mut dashboard, Vec::new());
    switch_to_hel(&mut dashboard);
    assert_eq!(titles(&rows(&dashboard)), ["ACP pretty name"]);
    switch_to_import(&mut dashboard);
    assert_eq!(rows(&dashboard).len(), 2, "an empty query lists the tab");

    // The index answers with the older native session first, a hit on the
    // Mjolnir record, and one archived session.
    replace_search(&mut dashboard, "phrase");
    apply_ready_rows(
        &mut dashboard,
        vec![
            WikiRow {
                tool: "codex".into(),
                native_id: Some("native-3".into()),
                snippet: Some("beta said the phrase".into()),
                ..wiki_row("beta-hit", false)
            },
            WikiRow {
                tool: "codex".into(),
                native_id: Some("native-2".into()),
                snippet: Some("alpha said the phrase".into()),
                ..wiki_row("alpha-hit", false)
            },
            WikiRow {
                hel_session_id: Some("session-1".into()),
                ..wiki_row("record-hit", false)
            },
            wiki_row("gone", true),
        ],
    );

    assert_eq!(
        titles(&rows(&dashboard)),
        ["Native beta", "Native alpha"],
        "the Import tab keeps the index's ranking, not newest first"
    );
    assert!(
        rows(&dashboard)[0].details.contains("beta said the phrase"),
        "each row carries its snippet: {}",
        rows(&dashboard)[0].details
    );

    switch_to_hel(&mut dashboard);
    assert_eq!(titles(&rows(&dashboard)), ["ACP pretty name"]);
    switch_to_archive(&mut dashboard);
    assert_eq!(titles(&rows(&dashboard)), ["archived gone"]);

    // A query the index does not match empties the tab, even though the
    // row's own text carries the word.
    replace_search(&mut dashboard, "native");
    apply_ready_rows(&mut dashboard, Vec::new());
    switch_to_tab(&mut dashboard, ResumeTab::Import);
    assert!(
        rows(&dashboard).is_empty(),
        "the local text of a row is no longer a search path"
    );
}

/// A search answer can put a row with a transcript under an unmoved
/// selection. The preview pane promises that transcript, so the dialog
/// asks for it without waiting for the selection to move.
#[test]
fn a_search_answer_asks_for_the_newly_selected_rows_transcript() {
    let mut dashboard = DashboardState::new(
        config(),
        state_with(vec![stopped_session()]),
        BTreeMap::new(),
    );
    dashboard.show_resume_dialog(1, vec![codex_profile(Vec::new())]);
    switch_to_hel(&mut dashboard);
    apply_ready_rows(&mut dashboard, Vec::new());
    assert_eq!(
        dashboard.next_wiki_preview(),
        DashboardAction::None,
        "a row with no indexed session has no transcript to show"
    );

    apply_ready_rows(
        &mut dashboard,
        vec![WikiRow {
            hel_session_id: Some("session-1".into()),
            snippet: Some("the phrase that matched".into()),
            ..wiki_row("held", false)
        }],
    );
    assert_eq!(
        dashboard.next_wiki_preview(),
        DashboardAction::LoadArchivedBrief {
            wiki_id: "held".to_owned()
        }
    );
    assert_eq!(
        dashboard.next_wiki_preview(),
        DashboardAction::None,
        "one request per row, not one per answer"
    );
}

/// An ongoing build leaves the query editable across tabs and returns the
/// indexed matches while polling for more.
#[test]
fn search_remains_available_while_the_index_builds() {
    let mut dashboard = DashboardState::new(
        config(),
        state_with(vec![stopped_session()]),
        BTreeMap::new(),
    );
    dashboard.show_resume_dialog(1, vec![codex_profile(Vec::new())]);
    replace_search(&mut dashboard, "archived");
    switch_to_hel(&mut dashboard);
    let request_id = match &dashboard.mode {
        Mode::ResumeDialog(dialog) => dialog.wiki_request_id,
        _ => panic!("expected the resume dialog"),
    };
    dashboard.apply_wiki_search(
        request_id,
        WikiSearchPage {
            rows: vec![wiki_row("gone", true)],
            status: WikiStatus {
                state: WikiIndexState::Indexing,
                topping_up: true,
            },
        },
    );
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert!(dialog.search_enabled());
    assert_eq!(dialog.search_placeholder(), None);

    // Tabs preserve the query and the already indexed matches.
    dashboard.handle_key(key(KeyCode::Char('/')));
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert_eq!(
        dialog.focused(),
        ResumeFocus::Search,
        "search remains editable during indexing"
    );
    switch_to_archive(&mut dashboard);
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert_eq!(dialog.tab, ResumeTab::Archive);
    assert_eq!(dialog.search.value(), "archived");
    assert_eq!(titles(&rows(&dashboard)), ["archived gone"]);
    let rendered = drawn(&mut dashboard, 120, 34);
    let search_line = rendered
        .iter()
        .find(|line| line.contains("Search:"))
        .unwrap();
    assert!(search_line.contains("archived"), "{search_line}");
    assert!(!search_line.contains("Indexing"), "{search_line}");

    // The build says it is still running, so the dialog asks again.
    let (next_id, query, delay) = dashboard
        .next_wiki_refresh()
        .expect("a building index is asked again");
    assert_eq!(query, "archived");
    assert_eq!(titles(&rows(&dashboard)), ["archived gone"]);
    assert_eq!(delay, WIKI_INDEXING_POLL);

    dashboard.apply_wiki_search(next_id, ready_page(vec![wiki_row("gone", true)]));
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert!(
        dialog.search_enabled(),
        "search stays available after the build finishes"
    );
    assert_eq!(dialog.search_placeholder(), None);
    assert!(
        dashboard.next_wiki_refresh().is_none(),
        "a ready, idle index is not polled"
    );
    assert_eq!(titles(&rows(&dashboard)), ["archived gone"]);
}

/// An index at another schema version says so and is never polled.
#[test]
fn a_version_mismatch_disables_the_box_and_stops_the_polling() {
    let mut dashboard = DashboardState::new(
        config(),
        state_with(vec![stopped_session()]),
        BTreeMap::new(),
    );
    dashboard.show_resume_dialog(1, vec![codex_profile(Vec::new())]);
    // Search on the history tabs is the index's answer, so they are where a
    // box that cannot answer yet says so.
    switch_to_hel(&mut dashboard);
    let request_id = match &dashboard.mode {
        Mode::ResumeDialog(dialog) => dialog.wiki_request_id,
        _ => panic!("expected the resume dialog"),
    };
    dashboard.apply_wiki_search(
        request_id,
        WikiSearchPage {
            rows: Vec::new(),
            status: WikiStatus {
                state: WikiIndexState::VersionMismatch,
                topping_up: false,
            },
        },
    );
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert!(!dialog.search_enabled());
    assert_eq!(
        dialog.search_placeholder(),
        Some("SessionWiki index is at a different version")
    );
    assert!(dashboard.next_wiki_refresh().is_none());
}

/// A sync that is adding rows makes the dialog repeat the query on a
/// lengthening schedule, and keeps repeating for as long as the sync runs
/// rather than abandoning a long one part way through.
#[test]
fn a_running_top_up_repeats_the_query_on_a_lengthening_schedule() {
    let mut dashboard = DashboardState::new(
        config(),
        state_with(vec![stopped_session()]),
        BTreeMap::new(),
    );
    dashboard.show_resume_dialog(1, vec![codex_profile(Vec::new())]);
    let topping_up = |rows: Vec<WikiRow>| WikiSearchPage {
        rows,
        status: WikiStatus {
            state: WikiIndexState::Ready,
            topping_up: true,
        },
    };
    let mut request_id = match &dashboard.mode {
        Mode::ResumeDialog(dialog) => dialog.wiki_request_id,
        _ => panic!("expected the resume dialog"),
    };
    // The schedule runs out and then repeats its last step: a sync longer
    // than the schedule is still followed.
    for attempt in 0..WIKI_TOP_UP_BACKOFF.len() + 3 {
        dashboard.apply_wiki_search(request_id, topping_up(Vec::new()));
        let (next_id, _, delay) = dashboard
            .next_wiki_refresh()
            .unwrap_or_else(|| panic!("repeat {attempt} was not scheduled"));
        assert_eq!(
            delay,
            WIKI_TOP_UP_BACKOFF[attempt.min(WIKI_TOP_UP_BACKOFF.len() - 1)]
        );
        request_id = next_id;
    }

    // An answer that says the sync has finished ends the repeats.
    dashboard.apply_wiki_search(request_id, ready_page(Vec::new()));
    assert!(
        dashboard.next_wiki_refresh().is_none(),
        "a finished sync is not polled"
    );

    // Typing starts the schedule again.
    replace_search(&mut dashboard, "something else");
    let (request_id, query) = dashboard.next_wiki_search().expect("a search is asked for");
    assert_eq!(query, "something else");
    dashboard.apply_wiki_search(request_id, topping_up(Vec::new()));
    let (_, _, delay) = dashboard.next_wiki_refresh().expect("the sync is followed");
    assert_eq!(delay, WIKI_TOP_UP_BACKOFF[0]);
}

/// Cost of the merged row list and of one keypress, on a dialog the size a
/// long-lived harness home produces. Run with
/// `cargo test -p brokk-mj-tui resume_row_cost -- --ignored --nocapture`.
#[test]
#[ignore = "timing measurement, not a behavior assertion"]
fn resume_row_cost_for_a_few_thousand_sessions() {
    const NATIVE: usize = 4_000;
    const RECORDS: usize = 400;
    const ROUNDS: usize = 200;

    let records = (0..RECORDS)
        .map(|index| {
            let mut session = stopped_session();
            session.id = format!("session-{index:04}");
            session.native_session_id = None;
            session.acp_session_title = Some(format!("Record {index}"));
            session
        })
        .collect::<Vec<_>>();
    let sessions = (0..NATIVE)
        .map(|index| {
            native(
                &format!("native-{index:04}"),
                &format!("Native conversation {index}"),
                index as i64,
            )
        })
        .collect::<Vec<_>>();
    let mut dashboard = DashboardState::new(config(), state_with(records), BTreeMap::new());
    dashboard.show_resume_dialog(1, vec![codex_profile(sessions)]);
    switch_to_import(&mut dashboard);

    let Mode::ResumeDialog(dialog) = dashboard.mode.clone() else {
        panic!("expected the resume dialog");
    };
    // What one rebuild costs: the merge, the sizes, and search.
    let started = Instant::now();
    let mut built = 0;
    for _ in 0..ROUNDS {
        built += build_resume_rows(
            &dashboard.config,
            &dashboard.state,
            &dialog,
            &dashboard.checkpoint_archive_sizes,
        )
        .0
        .len();
    }
    let rebuild = started.elapsed();

    // What the dialog actually pays per key press, which reads the rows
    // rather than rebuilding them.
    let started = Instant::now();
    for _ in 0..ROUNDS {
        dashboard.handle_key(key(KeyCode::Down));
    }
    let keypresses = started.elapsed();

    println!(
        "rows={} rebuild={:?} per_rebuild={:?} keypresses={:?} per_key={:?}",
        built / ROUNDS,
        rebuild,
        rebuild / ROUNDS as u32,
        keypresses,
        keypresses / ROUNDS as u32,
    );
}

/// The in-dialog keys are advertised where the user can see them.
#[test]
fn the_dialog_footer_names_its_own_keys() {
    let mut dashboard = DashboardState::new(
        config(),
        state_with(vec![stopped_session()]),
        BTreeMap::new(),
    );
    dashboard.show_resume_dialog(1, Vec::new());
    let mut terminal = Terminal::new(TestBackend::new(120, 34)).expect("terminal");
    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .expect("draw the Live tab");
    let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
    for hint in ["Enter opens", "a/b/w/i/d filter"] {
        assert!(rendered.contains(hint), "{rendered}");
    }
    assert!(!rendered.contains("destroys"), "{rendered}");

    switch_to_hel(&mut dashboard);
    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .expect("draw the Mjolnir tab");
    let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
    for hint in [
        "Mjolnir",
        "Import",
        "Delete destroys",
        "  Destroy  ",
        "←/→ tabs",
        "/ searches",
    ] {
        assert!(rendered.contains(hint), "{rendered}");
    }
    // The dashboard's own footer row is drawn behind the modal and names the
    // Sessions filter letters, so read the dialog's hint row rather than the
    // whole screen.
    let dialog_hints = buffer_lines(terminal.backend().buffer())
        .into_iter()
        .find(|line| line.contains("←/→ tabs"))
        .expect("the dialog's hint row");
    assert!(!dialog_hints.contains("filter"), "{dialog_hints}");

    switch_to_import(&mut dashboard);
    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .expect("draw the Import tab");
    let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
    assert!(rendered.contains("Enter imports"), "{rendered}");
    assert!(!rendered.contains("destroys"), "{rendered}");
    assert!(!rendered.contains("  Destroy  "), "{rendered}");
    assert!(!rendered.contains("archives"), "{rendered}");
}

/// A query's hits are counted on every tab, not only the one on screen, and
/// an empty tab says where the other hits are rather than switching by itself.
#[test]
fn sub_agents_are_never_offered_for_resume() {
    let owner = stopped_session();
    let managed_child = SessionRecord {
        id: "child-1".into(),
        acp_session_title: Some("Managed lane".into()),
        native_session_id: Some("native-child".into()),
        ..stopped_session()
    };
    let mut state = state_with(vec![owner.clone(), managed_child.clone()]);
    state.subagents.insert(
        managed_child.id.clone(),
        mj_core::subagent::SubagentRecord {
            child_session_id: managed_child.id.clone(),
            parent_session_id: owner.id.clone(),
            task_name: "lane".into(),
            profile_id: managed_child.last_profile.clone(),
            model: None,
            effort: None,
            working_directory: std::path::PathBuf::new(),
            initial_prompt: "work in the lane".into(),
            request_key: "request-1".into(),
            created_at: managed_child.created_at.clone(),
            noticed_turn: None,
        },
    );
    let mut dashboard = DashboardState::new(config(), state, BTreeMap::new());
    // A harness-owned child the owner spawned, shown the way the TUI shows it:
    // as a stopped copy of the owner's record.
    let agent = mj_core::native_agent::NativeAgent {
        owner_session_id: owner.id.clone(),
        session_id: "a0c7080aee7ead7c5".into(),
        parent_session_id: None,
        name: "Regression: bridge derivation conflict".into(),
        task: "Fix the regression".into(),
        capabilities: Default::default(),
        state: mj_core::native_agent::NativeAgentState::Completed,
        availability: Default::default(),
        availability_reason: None,
        stable_id: None,
    };
    dashboard.set_native_agents(vec![mj_core::native_agent::NativeAgentView {
        generation_ordinal: 1,
        projection: mj_core::state::MaterializedSession::empty(agent.view_id()),
        agent,
    }]);
    dashboard.show_resume_dialog(1, Vec::new());
    switch_to_hel(&mut dashboard);

    assert_eq!(titles(&rows(&dashboard)), ["ACP pretty name"]);

    // A search hit on a child counts on no tab.
    replace_search(&mut dashboard, "the phrase");
    apply_ready_rows(
        &mut dashboard,
        vec![
            WikiRow {
                hel_session_id: Some(managed_child.id.clone()),
                ..wiki_row("child-hit", false)
            },
            WikiRow {
                hel_session_id: Some(owner.id.clone()),
                ..wiki_row("owner-hit", false)
            },
        ],
    );
    assert_eq!(dashboard.resume_hit_counts, [0, 1, 0, 0]);
    assert_eq!(titles(&rows(&dashboard)), ["ACP pretty name"]);
}

#[test]
fn search_counts_hits_on_every_tab() {
    let mut dashboard = DashboardState::new(
        config(),
        state_with(vec![stopped_session()]),
        BTreeMap::new(),
    );
    dashboard.show_resume_dialog(
        1,
        vec![codex_profile(vec![
            native("native-2", "Native alpha", NEWER_THAN_THE_CHECKPOINT),
            native("native-3", "Native beta", NEWER_THAN_THE_CHECKPOINT - 1),
        ])],
    );
    switch_to_hel(&mut dashboard);
    replace_search(&mut dashboard, "the phrase");
    apply_ready_rows(
        &mut dashboard,
        vec![
            WikiRow {
                tool: "codex".into(),
                native_id: Some("native-2".into()),
                ..wiki_row("alpha-hit", false)
            },
            WikiRow {
                tool: "codex".into(),
                native_id: Some("native-3".into()),
                ..wiki_row("beta-hit", false)
            },
            WikiRow {
                hel_session_id: Some("session-1".into()),
                ..wiki_row("record-hit", false)
            },
            wiki_row("gone", true),
        ],
    );

    assert_eq!(
        dashboard.resume_hit_counts,
        [0, 1, 2, 1],
        "every tab's hits are counted, whichever tab is showing"
    );
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert_eq!(
        resume_tab_labels(&dashboard, dialog),
        [" Live ", " Mjolnir · 1 ", " Import · 2 ", " Archived · 1 "]
    );

    // A query only the Import rows match leaves the other tabs empty, and
    // they say where the hits are instead of moving the person.
    apply_ready_rows(
        &mut dashboard,
        vec![WikiRow {
            tool: "codex".into(),
            native_id: Some("native-2".into()),
            ..wiki_row("alpha-hit", false)
        }],
    );
    assert_eq!(dashboard.resume_hit_counts, [0, 0, 1, 0]);
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert!(rows(&dashboard).is_empty(), "the Mjolnir tab has no hits");
    assert_eq!(
        empty_search_message(&dashboard, dialog),
        "No matches here · 1 on Import"
    );
    assert_eq!(dialog.tab, ResumeTab::Hel, "the dialog never switches tabs");
}

/// One archived row per index, so the preview pane has a row to sit under and
/// the list has enough rows to scroll.
fn archived_rows(count: usize) -> Vec<WikiRow> {
    (0..count)
        .map(|index| WikiRow {
            started: Some(format!("2026-09-{:02}T00:00:00Z", index + 1)),
            ..wiki_row(&format!("archive-{index}"), true)
        })
        .collect()
}

/// The wheel moves whichever pane it is over: the preview scrolls under the
/// pointer and stops at the end of the text, and the list still moves when the
/// pointer is over the list.
#[test]
fn the_wheel_scrolls_the_preview_pane_it_is_over() {
    let mut dashboard = DashboardState::new(
        config(),
        state_with(vec![stopped_session()]),
        BTreeMap::new(),
    );
    dashboard.show_resume_dialog(1, vec![codex_profile(Vec::new())]);
    apply_ready_rows(&mut dashboard, archived_rows(12));
    switch_to_archive(&mut dashboard);
    let selected = rows(&dashboard)[0]
        .wiki_id()
        .expect("an archived row previews its own transcript")
        .to_owned();
    dashboard.apply_wiki_brief(
        selected,
        (0..120)
            .map(|index| format!("transcript line {index}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );

    let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("terminal");
    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .expect("draw the resume dialog");
    let preview = dashboard
        .frame_surfaces()
        .surface(mj_chat::selection::SurfaceId::ResumePreview)
        .copied()
        .expect("the preview pane registers a scrollable surface");
    let list = dashboard
        .frame_surfaces()
        .surface(mj_chat::selection::SurfaceId::ResumeList)
        .copied()
        .expect("the list registers its surface");
    assert!(
        preview.total_rows > usize::from(preview.rect.height),
        "the fixture overflows the pane"
    );

    dashboard.handle_mouse(mouse_in(MouseEventKind::ScrollDown, preview.rect));
    let scroll = |dashboard: &DashboardState| match &dashboard.mode {
        Mode::ResumeDialog(dialog) => dialog.preview_scroll,
        _ => panic!("expected the resume dialog"),
    };
    assert_eq!(scroll(&dashboard), 3, "one notch moves three rows");

    for _ in 0..200 {
        dashboard.handle_mouse(mouse_in(MouseEventKind::ScrollDown, preview.rect));
    }
    let end = preview
        .total_rows
        .saturating_sub(usize::from(preview.rect.height));
    assert_eq!(scroll(&dashboard), end, "the pane stops at the end");

    // The wheel over the list still moves the list, and leaves the pane where
    // the reader left it.
    let before = scroll(&dashboard);
    dashboard.handle_mouse(mouse_in(MouseEventKind::ScrollDown, list.rect));
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert_eq!(dialog.row_index, 1, "the list moved under the pointer");
    // A different transcript under the selection opens at its top.
    assert_eq!(dialog.preview_scroll, 0);
    assert!(before > 0);
}

#[test]
fn the_preview_scrollbar_seeks_and_drags_to_both_ends() {
    let mut dashboard = DashboardState::new(config(), state_with(Vec::new()), BTreeMap::new());
    dashboard.show_resume_dialog(1, vec![codex_profile(Vec::new())]);
    apply_ready_rows(&mut dashboard, archived_rows(1));
    switch_to_archive(&mut dashboard);
    dashboard.apply_wiki_brief(
        "archive-0".into(),
        (0..120)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    drawn(&mut dashboard, 120, 40);
    let geometry = match &dashboard.mode {
        Mode::ResumeDialog(dialog) => dialog.preview_scrollbar.borrow().geometry().unwrap(),
        _ => panic!("expected the resume dialog"),
    };
    let at = |kind, row| mouse_at(kind, (geometry.track.x, row));
    assert!(dashboard.component_handles_mouse(at(
        MouseEventKind::Down(MouseButton::Left),
        geometry.track.y
    )));
    dashboard.handle_mouse(at(
        MouseEventKind::Down(MouseButton::Left),
        geometry.track.y,
    ));
    dashboard.handle_mouse(at(MouseEventKind::Drag(MouseButton::Left), u16::MAX));
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert_eq!(dialog.preview_scroll, geometry.max_scroll);
    dashboard.handle_mouse(at(MouseEventKind::Drag(MouseButton::Left), 0));
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert_eq!(dialog.preview_scroll, 0);
    dashboard.handle_mouse(at(MouseEventKind::Up(MouseButton::Left), 0));
    assert!(!dashboard.component_handles_mouse(at(MouseEventKind::Drag(MouseButton::Left), 0)));
}

/// A search that is still running says so, and only the answer to the
/// outstanding request ends the wait.
#[test]
fn search_reply_clears_pending_for_matching_request_only() {
    let mut dashboard = DashboardState::new(
        config(),
        state_with(vec![stopped_session()]),
        BTreeMap::new(),
    );
    dashboard.show_resume_dialog(1, vec![codex_profile(Vec::new())]);
    replace_search(&mut dashboard, "the phrase");
    let (request_id, _) = dashboard.next_wiki_search().expect("a search is asked for");
    let pending = |dashboard: &DashboardState| match &dashboard.mode {
        Mode::ResumeDialog(dialog) => dialog.wiki_pending,
        _ => panic!("expected the resume dialog"),
    };
    assert!(pending(&dashboard));
    assert!(
        dashboard.needs_fast_tick(),
        "the searching spinner animates while the answer is outstanding"
    );

    dashboard.apply_wiki_search(request_id.wrapping_sub(1), ready_page(Vec::new()));
    assert!(
        pending(&dashboard),
        "an answer to an older query does not end this wait"
    );

    dashboard.apply_wiki_search(request_id, ready_page(Vec::new()));
    assert!(!pending(&dashboard));
    assert!(!dashboard.needs_fast_tick());
}

/// One matching message: `filler` lines of context above the match, so two
/// blocks put their hits far apart in the pane.
fn hit_block(role: &str, filler: usize) -> WikiHitBlock {
    let mut text = String::new();
    for index in 0..filler {
        text.push_str(&format!("filler line {index}\n"));
    }
    let start = text.len();
    text.push_str("needle");
    WikiHitBlock {
        role: role.to_owned(),
        hits: vec![(start, text.len())],
        text,
        omitted_before: 0,
        truncated: false,
    }
}

/// `n` moves the preview pane onto the next match, which sits far enough down
/// the excerpt that it was off screen before the keystroke.
#[test]
fn n_moves_the_preview_pane_to_the_next_hit() {
    let mut dashboard = DashboardState::new(
        config(),
        state_with(vec![stopped_session()]),
        BTreeMap::new(),
    );
    dashboard.show_resume_dialog(1, vec![codex_profile(Vec::new())]);
    apply_ready_rows(&mut dashboard, archived_rows(3));
    switch_to_archive(&mut dashboard);
    replace_search(&mut dashboard, "needle");
    apply_ready_rows(&mut dashboard, archived_rows(3));
    let selected = match &dashboard.mode {
        Mode::ResumeDialog(dialog) => dialog
            .preview_wiki_id(dashboard.resume_rows())
            .expect("an archived row previews its own transcript")
            .to_owned(),
        _ => panic!("expected the resume dialog"),
    };
    dashboard.apply_wiki_hits(
        selected,
        "needle".to_owned(),
        Some(WikiHitTranscript {
            blocks: vec![hit_block("user", 0), hit_block("assistant", 60)],
            omitted_after: 2,
        }),
    );

    let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("terminal");
    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .expect("draw the resume dialog");

    // Where each hit lands once the pane has wrapped the excerpt to its own
    // width, and which of those rows the pane is showing.
    let hits_and_view = |dashboard: &DashboardState| {
        let Mode::ResumeDialog(dialog) = &dashboard.mode else {
            panic!("expected the resume dialog");
        };
        let (lines, hit_lines) = dialog
            .preview_body(dashboard.resume_rows())
            .expect("the pane shows the matching passages");
        let surface = dashboard
            .frame_surfaces()
            .surface(SurfaceId::ResumePreview)
            .copied()
            .expect("the preview pane registers a scrollable surface");
        let width = usize::from(surface.rect.width);
        let rows = hit_lines
            .iter()
            .map(|&logical| wrap_preview_lines(&lines[..logical], width).len())
            .collect::<Vec<_>>();
        let view = dialog.preview_scroll..dialog.preview_scroll + usize::from(surface.rect.height);
        (rows, view)
    };

    let (hit_rows, view) = hits_and_view(&dashboard);
    assert_eq!(hit_rows.len(), 2, "the fixture has two matches");
    assert!(
        view.contains(&hit_rows[0]),
        "the pane opens on the first hit"
    );
    assert!(
        !view.contains(&hit_rows[1]),
        "the second match is below the pane"
    );

    focus_resume_control(&mut dashboard, ResumeFocus::Sessions);
    dashboard.handle_key(key(KeyCode::Char('n')));
    let (hit_rows, view) = hits_and_view(&dashboard);
    assert!(view.contains(&hit_rows[1]), "n moved the pane to the match");
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert_eq!(dialog.preview_hit, 1);
    assert!(dialog.preview_scroll > 0, "the pane scrolled to get there");

    let lines = drawn(&mut dashboard, 120, 40);
    let previous = point(&lines, "[↑]");
    dashboard.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), previous));
    dashboard.handle_mouse(mouse_at(MouseEventKind::Up(MouseButton::Left), previous));
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert_eq!(
        dialog.preview_hit, 0,
        "the up arrow selects the preceding hit"
    );

    let lines = drawn(&mut dashboard, 120, 40);
    let next = point(&lines, "[↓]");
    dashboard.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), next));
    dashboard.handle_mouse(mouse_at(MouseEventKind::Up(MouseButton::Left), next));
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert_eq!(dialog.preview_hit, 1, "the down arrow selects the next hit");
}

/// The pane asks for what it shows: the query's matching passages while a
/// query is active, the briefing when there is none, and a fresh answer for
/// each new query.
#[test]
fn a_query_asks_for_hits_and_no_query_asks_for_a_brief() {
    let mut dashboard = DashboardState::new(
        config(),
        state_with(vec![stopped_session()]),
        BTreeMap::new(),
    );
    dashboard.show_resume_dialog(1, vec![codex_profile(Vec::new())]);
    apply_ready_rows(&mut dashboard, archived_rows(1));
    switch_to_archive(&mut dashboard);
    let pending = match &dashboard.mode {
        Mode::ResumeDialog(dialog) => dialog.preview_pending.clone(),
        _ => panic!("expected the resume dialog"),
    };
    assert_eq!(
        pending,
        Some("archive-0".to_owned()),
        "with nothing typed the pane asks for the briefing"
    );

    replace_search(&mut dashboard, "needle");
    apply_ready_rows(&mut dashboard, archived_rows(1));
    let hits = DashboardAction::LoadArchivedHits {
        wiki_id: "archive-0".to_owned(),
        query: "needle".to_owned(),
    };
    assert_eq!(dashboard.next_wiki_preview(), hits);
    assert_eq!(
        dashboard.next_wiki_preview(),
        DashboardAction::None,
        "one request per query, not one per answer"
    );
    dashboard.apply_wiki_hits(
        "archive-0".to_owned(),
        "needle".to_owned(),
        Some(WikiHitTranscript::default()),
    );
    assert_eq!(
        dashboard.next_wiki_preview(),
        DashboardAction::None,
        "a cached answer is not asked for again"
    );

    replace_search(&mut dashboard, "other");
    apply_ready_rows(&mut dashboard, archived_rows(1));
    assert_eq!(
        dashboard.next_wiki_preview(),
        DashboardAction::LoadArchivedHits {
            wiki_id: "archive-0".to_owned(),
            query: "other".to_owned(),
        },
        "a new query is a new answer"
    );
}

/// One message with no match, so a fixture can place plain blocks of a given
/// role around the ones that match.
fn plain_block(role: &str, text: &str) -> WikiHitBlock {
    WikiHitBlock {
        role: role.to_owned(),
        hits: Vec::new(),
        text: text.to_owned(),
        omitted_before: 0,
        truncated: false,
    }
}

/// A run of tool messages is one `[tool calls]` line however long the run is,
/// and the two conversational roles carry the colours the conversation view
/// gives them, so a reader moves between the two surfaces without relearning
/// them.
#[test]
fn the_preview_collapses_tool_runs_and_colours_the_conversation_roles() {
    let (lines, _) = hit_transcript_lines(&WikiHitTranscript {
        blocks: vec![
            plain_block("user", "make it build"),
            plain_block("tool", "cargo build"),
            plain_block("tool", "cargo clippy"),
            plain_block("tool", "cargo test"),
            plain_block("assistant", "it builds"),
            plain_block("tool", "git commit"),
        ],
        omitted_after: 0,
    });
    let text = |line: &Line<'static>| {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    };
    let rendered: Vec<String> = lines.iter().map(text).collect();

    assert_eq!(
        rendered,
        vec![
            "User: make it build",
            "",
            "[tool calls]",
            "",
            "Assistant: it builds",
            "",
            "[tool calls]",
        ],
        "three tool messages in a row are one line, and a later run is its own"
    );

    let line_for = |needle: &str| {
        lines
            .iter()
            .find(|line| text(line).starts_with(needle))
            .expect("a line for the role")
    };
    // The role label is one span in front of the message text, so its colour
    // is the span's; the collapsed run is a whole styled line.
    assert_eq!(
        line_for("User: ").spans[0].style.fg,
        Some(theme::palette().accent)
    );
    assert_eq!(
        line_for("Assistant: ").spans[0].style.fg,
        Some(theme::palette().secondary)
    );
    assert_eq!(
        line_for("[tool calls]").style.fg,
        Some(theme::palette().muted)
    );
}

/// A collapsed run does not swallow the count of messages skipped before the
/// group it opens: the marker still separates the two runs it sits between.
#[test]
fn an_omission_between_two_tool_runs_keeps_both_runs_visible() {
    let (lines, _) = hit_transcript_lines(&WikiHitTranscript {
        blocks: vec![
            plain_block("tool", "cargo build"),
            WikiHitBlock {
                omitted_before: 4,
                ..plain_block("tool", "cargo test")
            },
        ],
        omitted_after: 0,
    });
    let rendered: Vec<String> = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect();

    assert_eq!(
        rendered,
        vec![
            "[tool calls]",
            "",
            "*[… 4 messages omitted …]*",
            "[tool calls]",
        ]
    );
}
