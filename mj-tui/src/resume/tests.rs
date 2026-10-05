use std::collections::BTreeMap;

use crossterm::event::KeyCode;
use mj_core::state::STATE_VERSION;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use super::*;
use crate::DashboardState;
use crate::test_support::*;

/// Rows for `state` as the dialog builds them once the daemon has answered
/// with the stopped sessions in it.
fn merged_rows(
    config: &Config,
    state: &State,
    profiles: &[ImportProfileOption],
    wiki: &[WikiRow],
) -> Vec<ResumeRow> {
    let history = ResumeHistory::from(resume_candidates_for(config, state));
    merged_resume_rows(
        config,
        state,
        &HistoryLoad::Loaded(Arc::new(history)),
        profiles,
        wiki,
    )
}

/// Later than `stopped_session`'s checkpoint, so a native row built with
/// it sorts above the Hel record.
const NEWER_THAN_THE_CHECKPOINT: i64 = 4_000_000_000_000;

fn native(id: &str, title: &str, last_activity_ms: i64) -> crate::ImportSessionOption {
    crate::ImportSessionOption {
        native_session_id: id.into(),
        title: title.into(),
        cwd: "/home/dev/Projects/hel".into(),
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

fn append_golden_state(
    output: &mut String,
    label: &str,
    width: u16,
    height: u16,
    lines: &[String],
) {
    output.push_str(&format!("=== {label} ({width}x{height}) ===\n"));
    output.push_str(&lines.join("\n"));
    output.push('\n');
}

fn append_golden_value(output: &mut String, label: &str, value: impl std::fmt::Debug) {
    output.push_str(&format!("{label}: {value:?}\n"));
}

fn append_resume_golden(
    output: &mut String,
    label: &str,
    width: u16,
    height: u16,
    dashboard: &mut DashboardState,
) {
    let lines = drawn(dashboard, width, height);
    append_golden_state(output, label, width, height, &lines);
}

/// C-25: a long title gives way to the "[unavailable]" marker, so the marker
/// is always shown in full.
// Hard-won: 72f1adfb: Finding C-25 showed a long import title clipped the [unavailable] marker to [unavaila].
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
        open_resume_dialog(&mut dashboard, 1, vec![codex_profile(vec![session])]);
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
            open_resume_dialog(
                &mut dashboard,
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
        last_subagent_policy: Default::default(),
        subagents: Default::default(),
        version: STATE_VERSION,
        sessions: sessions
            .into_iter()
            .map(|session| (session.id.clone(), session))
            .collect(),
        mount_history: Default::default(),
        container_sizes: Default::default(),
    }
}

fn incomplete_move() -> MoveOperation {
    MoveOperation {
        prepared_destination: None,
        accepted_preparation: None,
        acknowledge_interruption: false,
        workspace_transfer: None,
        handoff: None,
        in_place: false,
        source_checkpoint_only: false,
        operation_id: "move-1".into(),
        selection: mj_core::state::MoveSelection {
            subagents: None,
            workspace: Default::default(),
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
    open_resume_dialog(&mut dashboard, 1, Vec::new());
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

#[test]
fn move_recovery_uses_recorded_policy_and_retained_explicit_override() {
    use mj_core::subagent::SubagentPolicy;
    for retained in [None, Some(SubagentPolicy::None)] {
        let mut session = running_session();
        session.subagents = Some(SubagentPolicy::AllModels);
        let mut configuration = config();
        configuration.profiles.get_mut("codex-1").unwrap().subagents =
            SubagentPolicy::SingleModel {
                model: "creation-default".into(),
                effort: None,
            };
        let mut dashboard =
            DashboardState::new(configuration, state_with(vec![session]), BTreeMap::new());
        let mut operation = incomplete_move();
        operation.selection.subagents = retained.clone();
        operation.selection.target_template_id = Some("podman".into());
        dashboard.begin_move_recovery(operation);
        let Mode::Resume(wizard) = &dashboard.mode else {
            panic!("recovery wizard");
        };
        assert_eq!(
            wizard.subagents.policy,
            retained.clone().unwrap_or(SubagentPolicy::AllModels)
        );
        assert_eq!(wizard.subagent_change(&dashboard), retained);
        assert_eq!(
            dashboard.state.sessions["session-1"].subagents,
            Some(SubagentPolicy::AllModels)
        );
    }
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
    let merged = merged_rows(
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

/// The native session behind a live Hel session must not be offered as an
/// import: that would start a second Hel session on the same conversation.
#[test]
fn a_live_session_hides_its_native_counterpart_from_the_dialog() {
    let mut live = stopped_session();
    live.state = SessionState::Running;
    let merged = merged_rows(
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

/// R10-3: a `/clear` leaves the session's earlier native thread in the
/// harness home. The store names one thread per session: until the next
/// checkpoint the one from before `/clear`, after it the new one. So the
/// Import tab offered session C's pre-`/clear` thread, whose history C
/// still holds, and importing it would have duplicated that history. A
/// thread that ran in a session's own checkout is that session's.
// Hard-won: ebcd71d2: R10-3 showed a session’s pre- or post-/clear Codex thread could be offered as a second import of its own history.
#[test]
fn a_thread_from_a_mjolnir_sessions_own_checkout_is_not_offered_for_import() {
    let clone = std::path::PathBuf::from(
        "/home/dev/reverify-10/project/.mj/clones/0858ecafaa06ecd148aa8bcf70bba899",
    );
    let pre_clear = "01a0d8c9-2bc1-7041-9a64-4b8216558cdc";
    let post_clear = "01a0d8c9-cc35-7e81-943e-0c480730ac99";
    // Before and after the checkpoint that records the new thread.
    for (stored, state) in [
        (pre_clear, SessionState::Running),
        (post_clear, SessionState::Stopped),
    ] {
        let mut session = stopped_session();
        session.id = "0858ecafaa06ecd148aa8bcf70bba899".into();
        session.state = state;
        session.native_session_id = Some(stored.into());
        session.project_directory = Some(clone.clone());
        session.managed_worktree = Some(mj_core::state::ManagedWorktree {
            kind: mj_core::state::ManagedCheckoutKind::Clone,
            source_project_directory: "/home/dev/reverify-10/project".into(),
            source_repository: "/home/dev/reverify-10/project".into(),
            worktree_root: clone.clone(),
            branch: "main".into(),
            target: mj_core::state::ManagedWorktreeTarget::Local,
            base_commit: None,
        });
        let mut before = native(pre_clear, "Remember PINEAPPLE and reply OK", 10);
        before.cwd = clone.clone();
        let mut after = native(post_clear, "What word did I ask you to remember?", 20);
        after.cwd = clone.clone();
        let mut own = native("user-thread", "The user's own thread", 5);
        own.cwd = "/home/dev/reverify-10/project".into();
        let merged = merged_rows(
            &config(),
            &state_with(vec![session]),
            &[codex_profile(vec![before, after, own])],
            &[],
        );
        let importable = merged
            .iter()
            .filter(|row| matches!(row.key, ResumeRowKey::Native(..)))
            .map(|row| row.title.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            importable,
            ["The user's own thread"],
            "with {stored} stored"
        );
    }
}

/// A record whose target was renamed or removed from config still reports
/// the target it actually ran on.
/// One order across the merged list: newest activity first, whichever
/// source the row came from. Hel records date from their checkpoint,
/// native sessions from the file's modification time.
/// A record archived by an older Mjolnir version remains part of stopped
/// history. Archive state is retained in storage for compatibility, but
/// it no longer separates rows in the resume dialog.
/// Native archive metadata is retained on the row for display consumers,
/// while the resume dialog always lists the row and never writes back to
/// the provider.
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
        open_resume_dialog(&mut dashboard, 1, Vec::new());
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
/// Launch finding R6-4: the Mjolnir tab listed a suspended session its
/// harness had not named by its id (tmux/022), while the Destroy dialog and
/// the notices called it "project via fake". The Mjolnir and Live tabs now
/// list the title every other listing uses.
// Hard-won: dd35efff: Launch finding R6-4 showed unnamed suspended sessions displayed their ID instead of the created title.
#[test]
fn a_session_its_harness_has_not_named_is_listed_by_its_created_title() {
    let unnamed = |mut session: SessionRecord, id: &str, title: &str| {
        session.id = id.into();
        session.title = title.into();
        session.acp_session_title = None;
        session.session_title_override = None;
        session.native_session_id = None;
        session
    };
    let stopped = unnamed(
        stopped_session(),
        "f20058f83046cc4943a71686253e3661",
        "project via fake",
    );
    let running = unnamed(
        running_session(),
        "0a1b2c3d4e5f60718293a4b5c6d7e8f9",
        "sibling via fake",
    );
    let mut dashboard = DashboardState::new(
        config(),
        state_with(vec![stopped, running]),
        BTreeMap::new(),
    );
    open_resume_dialog(&mut dashboard, 1, Vec::new());

    assert_eq!(titles(&rows(&dashboard)), ["sibling via fake"]);
    let live = drawn(&mut dashboard, 140, 40);
    let row = live
        .iter()
        .find(|line| line.contains("sibling via fake"))
        .unwrap_or_else(|| panic!("no Live row names the session:\n{}", live.join("\n")));
    assert!(!row.contains("0a1b2c3d4e5f"), "{row}");

    switch_to_hel(&mut dashboard);
    assert_eq!(titles(&rows(&dashboard)), ["project via fake"]);
    let hel = drawn(&mut dashboard, 140, 40);
    let row = hel
        .iter()
        .find(|line| line.contains("project via fake"))
        .unwrap_or_else(|| panic!("no Mjolnir row names the session:\n{}", hel.join("\n")));
    assert!(!row.contains("f20058f83046"), "{row}");
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
    open_resume_dialog(&mut dashboard, 1, Vec::new());
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
    assert_eq!(Some(buffer[(active, y)].bg), theme::focus_control().bg);
    assert_eq!(buffer[(inactive, y)].bg, theme::palette().selection);
}

/// Hel never modifies a harness home, so a native-only row has no destroy action.
#[test]
fn a_native_only_row_cannot_be_destroyed_from_hel() {
    let mut dashboard = DashboardState::new(config(), state_with(Vec::new()), BTreeMap::new());
    open_resume_dialog(
        &mut dashboard,
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

/// C-12: when a state filter empties the Live list, the message names the
/// filter rather than saying nothing is running.
// Hard-won: db681e6e: Finding C-12 showed a filtered Live list claiming no sessions were running when the filter had no matches.
#[test]
fn an_empty_filtered_live_list_names_the_filter() {
    let mut dashboard = dashboard_with_session(running_session());
    open_resume_dialog(&mut dashboard, 1, Vec::new());
    dashboard.handle_key(key(KeyCode::Char('b')));
    assert!(rows(&dashboard).is_empty(), "no session is blocked");
    let rendered = drawn(&mut dashboard, 120, 40).join("\n");
    assert!(rendered.contains("No blocked sessions"), "{rendered}");
    assert!(!rendered.contains("No running sessions"), "{rendered}");
}

/// C-13: the archived briefing's UTC date is shown in local time, like every
/// other time on screen.
// Hard-won: 8b5f8bb4: Finding C-13 showed archived briefing timestamps in UTC while the rest of the UI used local time.
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
// Hard-won: 8e13ffb1: Finding C-6 showed Enter in the Live search box only moved focus instead of opening the selected match.
#[test]
fn enter_in_the_search_box_opens_the_live_match() {
    let mut dashboard = dashboard_with_live_sessions_in_two_workspaces();
    open_resume_dialog(&mut dashboard, 1, Vec::new());
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
    open_resume_dialog(&mut dashboard, 1, Vec::new());
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("expected the resume dialog");
    };
    assert_eq!(dialog.wiki_search, WikiSearchState::Pending);
    let focus_before = dialog_focus(&dashboard);

    apply_ready_rows(&mut dashboard, Vec::new());
    assert_eq!(dialog_focus(&dashboard), focus_before);
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

/// An id the configuration no longer has leaves the wizard on its first
/// choice; there is nothing better to offer.
#[test]
fn an_unknown_indexed_profile_leaves_the_restore_wizard_on_its_first_choice() {
    let mut dashboard = DashboardState::new(
        config_with_bare_target(),
        state_with(Vec::new()),
        BTreeMap::new(),
    );
    open_resume_dialog(&mut dashboard, 1, vec![codex_profile(Vec::new())]);
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
    open_resume_dialog(&mut dashboard, 1, vec![codex_profile(Vec::new())]);
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

/// Selecting a row dispatches to the flow that suits its source: the
/// resume wizard for a Hel record, the import flow for a native session.
/// Enter on the focused Mjolnir row starts its resume, which is what the tab's
/// own hint promises and what the Resume button already does.
// Hard-won: c82060f6: Enter on a Mjolnir row did nothing after arrow navigation left focus on the tab strip.
#[test]
fn enter_on_a_focused_mjolnir_row_begins_its_resume() {
    let mut dashboard = DashboardState::new(
        config(),
        state_with(vec![stopped_session()]),
        BTreeMap::new(),
    );
    open_resume_dialog(&mut dashboard, 1, Vec::new());
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
fn golden_resume_dialog_table() {
    let mut output = String::new();

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
    let mut project_config = config();
    project_config.targets.insert(
        "localhost".into(),
        mj_core::config::TargetTemplate::LocalBare,
    );
    project_config.targets.insert(
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
    let mut dashboard = DashboardState::new(
        project_config,
        state_with(vec![local, remote]),
        BTreeMap::new(),
    );
    open_resume_dialog(
        &mut dashboard,
        1,
        vec![codex_profile(vec![native(
            "native-only",
            "Native project",
            NEWER_THAN_THE_CHECKPOINT,
        )])],
    );
    switch_to_hel(&mut dashboard);
    append_resume_golden(
        &mut output,
        "Hel records with project targets",
        140,
        34,
        &mut dashboard,
    );
    switch_to_import(&mut dashboard);
    append_resume_golden(&mut output, "native import target", 140, 34, &mut dashboard);

    let mut session = stopped_session();
    session.target_template_id = "retired-target".into();
    let mut retired_config = config();
    retired_config.targets.clear();
    let mut dashboard =
        DashboardState::new(retired_config, state_with(vec![session]), BTreeMap::new());
    open_resume_dialog(&mut dashboard, 1, Vec::new());
    switch_to_hel(&mut dashboard);
    append_resume_golden(
        &mut output,
        "stored target after config removal",
        120,
        34,
        &mut dashboard,
    );

    let mut old_record = stopped_session();
    old_record.id = "old-record".into();
    old_record.native_session_id = None;
    old_record.checkpoint.as_mut().unwrap().created_at = "2026-01-01T00:00:00Z".into();
    let mut new_record = stopped_session();
    new_record.id = "new-record".into();
    new_record.native_session_id = None;
    new_record.acp_session_title = Some("Newest record".into());
    new_record.checkpoint.as_mut().unwrap().created_at = "2026-06-01T00:00:00Z".into();
    let merged = merged_rows(
        &config(),
        &state_with(vec![old_record, new_record]),
        &[codex_profile(vec![
            native("native-mid", "Native March", 1_772_409_600_000),
            native("native-new", "Native July", 1_782_950_400_000),
        ])],
        &[],
    );
    append_golden_value(
        &mut output,
        "ordered resume titles",
        merged
            .iter()
            .map(|row| row.title.as_str())
            .collect::<Vec<_>>(),
    );
    let mut dashboard = DashboardState::new(
        config(),
        state_with(vec![
            {
                let mut row = stopped_session();
                row.id = "old-record".into();
                row.native_session_id = None;
                row.checkpoint.as_mut().unwrap().created_at = "2026-01-01T00:00:00Z".into();
                row
            },
            {
                let mut row = stopped_session();
                row.id = "new-record".into();
                row.native_session_id = None;
                row.acp_session_title = Some("Newest record".into());
                row.checkpoint.as_mut().unwrap().created_at = "2026-06-01T00:00:00Z".into();
                row
            },
        ]),
        BTreeMap::new(),
    );
    open_resume_dialog(
        &mut dashboard,
        1,
        vec![codex_profile(vec![
            native("native-mid", "Native March", 1_772_409_600_000),
            native("native-new", "Native July", 1_782_950_400_000),
        ])],
    );
    switch_to_hel(&mut dashboard);
    append_resume_golden(
        &mut output,
        "cross-source activity order",
        140,
        34,
        &mut dashboard,
    );

    let mut archived = stopped_session();
    archived.id = "archived-record".into();
    archived.native_session_id = None;
    archived.acp_session_title = Some("Archived record".into());
    archived.archived = true;
    let mut current = stopped_session();
    current.id = "current-record".into();
    current.native_session_id = None;
    current.acp_session_title = Some("Current record".into());
    let mut dashboard = DashboardState::new(
        config(),
        state_with(vec![archived, current]),
        BTreeMap::new(),
    );
    open_resume_dialog(&mut dashboard, 1, Vec::new());
    switch_to_hel(&mut dashboard);
    append_resume_golden(
        &mut output,
        "history including older archived records",
        120,
        34,
        &mut dashboard,
    );

    let mut natively_archived = native("native-codex", "Archived in Codex", 1);
    natively_archived.natively_archived = true;
    let mut dashboard = DashboardState::new(config(), state_with(Vec::new()), BTreeMap::new());
    open_resume_dialog(
        &mut dashboard,
        1,
        vec![codex_profile(vec![natively_archived])],
    );
    switch_to_import(&mut dashboard);
    append_golden_value(
        &mut output,
        "native archive metadata",
        rows(&dashboard)[0].natively_archived,
    );
    append_resume_golden(
        &mut output,
        "native archived conversation",
        120,
        34,
        &mut dashboard,
    );
    let action = dashboard.handle_key(key(KeyCode::Char('a')));
    append_golden_value(&mut output, "archive shortcut action", action);
    append_resume_golden(
        &mut output,
        "native archived conversation remains listed",
        120,
        34,
        &mut dashboard,
    );

    let mut dashboard = DashboardState::new(
        config(),
        state_with(vec![stopped_session()]),
        BTreeMap::new(),
    );
    open_resume_dialog(&mut dashboard, 1, Vec::new());
    switch_to_hel(&mut dashboard);
    let action = dashboard.handle_key(key(KeyCode::Char('d')));
    append_golden_value(&mut output, "d key action", action);
    append_resume_golden(
        &mut output,
        "resume dialog before destroy button",
        120,
        34,
        &mut dashboard,
    );
    let lines = drawn(&mut dashboard, 120, 34);
    let destroy = point(&lines, "Destroy");
    dashboard.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), destroy));
    let action = dashboard.handle_mouse(mouse_at(MouseEventKind::Up(MouseButton::Left), destroy));
    append_golden_value(&mut output, "Destroy button action", action);
    append_resume_golden(&mut output, "destroy confirmation", 120, 34, &mut dashboard);

    let archive_config = config_with_bare_target();
    let state = state_with(Vec::new());
    let wiki_rows = vec![
        tagged_wiki_row("bare", "localhost", "codex-2"),
        WikiRow {
            project: "/home/dev/project/.mj/worktrees/556ebcbaee181".into(),
            ..tagged_wiki_row("worktree", "localhost", "codex-2")
        },
        WikiRow {
            project: "/home/dev/project/.mj/worktrees/556ebcbaee181/sub".into(),
            target: None,
            ..tagged_wiki_row("nested", "localhost", "codex-2")
        },
        tagged_wiki_row("container", "podman", "codex-2"),
        tagged_wiki_row("retired", "was-a-target", "codex-2"),
        wiki_row("untagged", true),
    ];
    let archive_rows = merged_rows(&archive_config, &state, &[], &wiki_rows);
    for row in &archive_rows {
        append_golden_value(
            &mut output,
            &format!("archived row {} profile/origin", row.title),
            (&row.profile_id, &row.origin),
        );
    }
    let mut dashboard = DashboardState::new(archive_config, state, BTreeMap::new());
    open_resume_dialog(&mut dashboard, 1, vec![codex_profile(Vec::new())]);
    switch_to_archive(&mut dashboard);
    apply_ready_rows(&mut dashboard, wiki_rows);
    append_resume_golden(
        &mut output,
        "archived rows with indexed profile and target",
        140,
        40,
        &mut dashboard,
    );

    let mut dashboard = DashboardState::new(
        config(),
        state_with(vec![stopped_session()]),
        BTreeMap::new(),
    );
    open_resume_dialog(
        &mut dashboard,
        1,
        vec![codex_profile(vec![native(
            "native-2",
            "Native",
            NEWER_THAN_THE_CHECKPOINT,
        )])],
    );
    switch_to_hel(&mut dashboard);
    append_resume_golden(&mut output, "Hel tab", 120, 34, &mut dashboard);
    switch_to_import(&mut dashboard);
    append_resume_golden(&mut output, "Import tab", 120, 34, &mut dashboard);
    dashboard.handle_key(key(KeyCode::Left));
    append_resume_golden(&mut output, "Left returns to Hel", 120, 34, &mut dashboard);

    let mut dashboard = DashboardState::new(
        config(),
        state_with(vec![stopped_session()]),
        BTreeMap::new(),
    );
    let profiles = || {
        vec![codex_profile(vec![native(
            "native-2",
            "Native",
            NEWER_THAN_THE_CHECKPOINT,
        )])]
    };
    open_resume_dialog(&mut dashboard, 1, profiles());
    switch_to_hel(&mut dashboard);
    append_resume_golden(
        &mut output,
        "select resumable Hel row",
        120,
        34,
        &mut dashboard,
    );
    let action = dashboard.handle_key(key(KeyCode::Enter));
    append_golden_value(&mut output, "resume selection action", action);
    append_golden_value(&mut output, "mode after selecting Hel row", "Resume wizard");
    append_resume_golden(&mut output, "resume wizard", 120, 34, &mut dashboard);

    open_resume_dialog(&mut dashboard, 2, profiles());
    switch_to_import(&mut dashboard);
    append_resume_golden(
        &mut output,
        "select importable native row",
        120,
        34,
        &mut dashboard,
    );
    let action = dashboard.handle_key(key(KeyCode::Enter));
    append_golden_value(&mut output, "native import action", action);
    append_resume_golden(
        &mut output,
        "dashboard after importing native row",
        120,
        34,
        &mut dashboard,
    );

    let mut dashboard = dashboard_with_live_sessions_in_two_workspaces();
    open_resume_dialog(&mut dashboard, 1, Vec::new());
    append_resume_golden(
        &mut output,
        "Live tab across workspaces",
        120,
        40,
        &mut dashboard,
    );
    focus_resume_control(&mut dashboard, ResumeFocus::Sessions);
    dashboard.handle_key(key(KeyCode::Left));
    append_resume_golden(
        &mut output,
        "left wraps to Archived tab",
        120,
        40,
        &mut dashboard,
    );
    switch_to_tab(&mut dashboard, ResumeTab::Live);
    let _ = drawn(&mut dashboard, 120, 40);
    let search_action = dashboard.handle_key(key(KeyCode::Char('/')));
    append_golden_value(&mut output, "focus Live search action", search_action);
    focus_resume_control(&mut dashboard, ResumeFocus::Search);
    for character in "mast".chars() {
        let action = dashboard.handle_key(key(KeyCode::Char(character)));
        append_golden_value(
            &mut output,
            &format!("search key {character} action"),
            action,
        );
    }
    let request = dashboard.resume_search_request_id().unwrap();
    dashboard.apply_resume_text_search_result(request, Ok(Vec::new()));
    append_resume_golden(&mut output, "Live search result", 120, 40, &mut dashboard);

    mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), "resume-dialog-table", &output);
}

#[test]
fn golden_resume_dialog_preview() {
    let mut output = String::new();

    let mut dashboard = DashboardState::new(
        config(),
        state_with(vec![stopped_session()]),
        BTreeMap::new(),
    );
    open_resume_dialog(
        &mut dashboard,
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
    append_resume_golden(
        &mut output,
        "search hits on each tab",
        120,
        40,
        &mut dashboard,
    );
    append_golden_value(&mut output, "tab hit counts", dashboard.resume_hit_counts);
    apply_ready_rows(
        &mut dashboard,
        vec![WikiRow {
            tool: "codex".into(),
            native_id: Some("native-2".into()),
            ..wiki_row("alpha-hit", false)
        }],
    );
    append_resume_golden(
        &mut output,
        "search result on another tab",
        120,
        40,
        &mut dashboard,
    );
    append_golden_value(
        &mut output,
        "tab hit counts after refresh",
        dashboard.resume_hit_counts,
    );
    if let Mode::ResumeDialog(dialog) = &dashboard.mode {
        append_golden_value(
            &mut output,
            "empty tab message",
            empty_search_message(&dashboard, dialog),
        );
    }

    let mut dashboard = DashboardState::new(
        config(),
        state_with(vec![stopped_session()]),
        BTreeMap::new(),
    );
    open_resume_dialog(&mut dashboard, 1, vec![codex_profile(Vec::new())]);
    apply_ready_rows(&mut dashboard, archived_rows(3));
    switch_to_archive(&mut dashboard);
    replace_search(&mut dashboard, "needle");
    apply_ready_rows(&mut dashboard, archived_rows(3));
    let selected = match &dashboard.mode {
        Mode::ResumeDialog(dialog) => dialog
            .preview_wiki_id(dashboard.resume_rows())
            .unwrap()
            .to_owned(),
        _ => unreachable!(),
    };
    dashboard.apply_wiki_hits(
        selected,
        "needle".into(),
        Some(WikiHitTranscript {
            blocks: vec![hit_block("user", 0), hit_block("assistant", 60)],
            omitted_after: 2,
        }),
    );
    append_resume_golden(
        &mut output,
        "preview opens at first matching passage",
        120,
        40,
        &mut dashboard,
    );
    focus_resume_control(&mut dashboard, ResumeFocus::Sessions);
    dashboard.handle_key(key(KeyCode::Char('n')));
    append_resume_golden(
        &mut output,
        "n selects and scrolls to second match",
        120,
        40,
        &mut dashboard,
    );
    let lines = drawn(&mut dashboard, 120, 40);
    let previous = point(&lines, "[↑]");
    dashboard.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), previous));
    dashboard.handle_mouse(mouse_at(MouseEventKind::Up(MouseButton::Left), previous));
    append_resume_golden(&mut output, "previous-hit arrow", 120, 40, &mut dashboard);
    let lines = drawn(&mut dashboard, 120, 40);
    let next = point(&lines, "[↓]");
    dashboard.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), next));
    dashboard.handle_mouse(mouse_at(MouseEventKind::Up(MouseButton::Left), next));
    append_resume_golden(&mut output, "next-hit arrow", 120, 40, &mut dashboard);

    let mut dashboard = DashboardState::new(
        config(),
        state_with(vec![stopped_session()]),
        BTreeMap::new(),
    );
    open_resume_dialog(&mut dashboard, 1, vec![codex_profile(Vec::new())]);
    apply_ready_rows(&mut dashboard, archived_rows(1));
    switch_to_archive(&mut dashboard);
    append_resume_golden(
        &mut output,
        "archived briefing without a query",
        120,
        40,
        &mut dashboard,
    );
    let action = dashboard.next_wiki_preview();
    append_golden_value(&mut output, "briefing request action", action);
    replace_search(&mut dashboard, "needle");
    apply_ready_rows(&mut dashboard, archived_rows(1));
    append_resume_golden(&mut output, "archived hit query", 120, 40, &mut dashboard);
    let action = dashboard.next_wiki_preview();
    append_golden_value(&mut output, "hit request action", action);
    let duplicate = dashboard.next_wiki_preview();
    append_golden_value(&mut output, "duplicate hit request", duplicate);
    dashboard.apply_wiki_hits(
        "archive-0".into(),
        "needle".into(),
        Some(WikiHitTranscript::default()),
    );
    let cached = dashboard.next_wiki_preview();
    append_golden_value(&mut output, "cached hit request", cached);
    replace_search(&mut dashboard, "other");
    apply_ready_rows(&mut dashboard, archived_rows(1));
    let action = dashboard.next_wiki_preview();
    append_golden_value(&mut output, "new query request action", action);
    append_resume_golden(
        &mut output,
        "new archived hit query",
        120,
        40,
        &mut dashboard,
    );

    mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), "resume-dialog-preview", &output);
}

fn type_resume_query(dashboard: &mut DashboardState, query: &str) -> u64 {
    drawn(dashboard, 120, 40);
    focus_resume_control(dashboard, ResumeFocus::Search);
    for character in query.chars() {
        dashboard.handle_key(key(KeyCode::Char(character)));
    }
    dashboard.resume_search_request_id().unwrap()
}

fn live_text_hit(session_id: &str) -> SessionTextMatch {
    SessionTextMatch {
        session_id: session_id.into(),
        kind: mj_client::daemon::SessionTextMatchKind::User,
    }
}

#[test]
fn live_conversation_matches_open_and_preview_without_a_history_page_hit() {
    let mut dashboard = dashboard_with_live_sessions_in_two_workspaces();
    dashboard
        .state
        .sessions
        .get_mut("live-remote")
        .unwrap()
        .session_title_override = Some("SSCD duplicate detection with voyage 4 nano".into());
    open_resume_dialog(&mut dashboard, 1, Vec::new());
    let request_id = type_resume_query(&mut dashboard, "rvector");
    assert!(rows(&dashboard).is_empty());

    // The limited history page can be empty or fail independently of Live.
    apply_ready_rows(&mut dashboard, Vec::new());
    assert!(
        dashboard
            .apply_resume_text_search_result(request_id, Ok(vec![live_text_hit("live-remote")]),)
    );
    assert_eq!(
        titles(&rows(&dashboard)),
        ["SSCD duplicate detection with voyage 4 nano"]
    );
    assert_eq!(
        dashboard.next_wiki_preview(),
        DashboardAction::LoadArchivedHits {
            wiki_id: "live-remote".into(),
            query: "rvector".into(),
        }
    );
    dashboard.apply_wiki_hits(
        "live-remote".into(),
        "rvector".into(),
        Some(WikiHitTranscript {
            blocks: vec![WikiHitBlock {
                hits: vec![(7, 14)],
                ..plain_block("user", "update rvector to use rq8")
            }],
            omitted_after: 0,
        }),
    );
    let rendered = drawn(&mut dashboard, 160, 40).join("\n");
    assert!(rendered.contains("update rvector to use rq8"), "{rendered}");
    dashboard.apply_wiki_search_result(request_id, Err("history unavailable".into()));
    assert_eq!(rows(&dashboard).len(), 1);
    assert!(
        !drawn(&mut dashboard, 160, 40)
            .join("\n")
            .contains("Search failed")
    );
    focus_resume_control(&mut dashboard, ResumeFocus::Sessions);
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::SelectWorkspace {
            workspace_id: "other".into()
        }
    );
    dashboard.set_active_workspace(Some("other".into()));
    assert_eq!(dashboard.selected_session_id(), Some("live-remote"));
}

#[test]
fn editing_the_query_removes_old_live_matches_and_rejects_delayed_answers() {
    let mut dashboard = dashboard_with_live_sessions_in_two_workspaces();
    open_resume_dialog(&mut dashboard, 1, Vec::new());
    let old = type_resume_query(&mut dashboard, "rvector");
    dashboard.apply_resume_text_search_result(old, Ok(vec![live_text_hit("live-remote")]));
    assert_eq!(rows(&dashboard).len(), 1);

    let current = type_resume_query(&mut dashboard, "x");
    assert!(
        rows(&dashboard).is_empty(),
        "old matches clear before debounce"
    );
    assert!(
        !dashboard.apply_resume_text_search_result(old, Ok(vec![live_text_hit("live-remote")]))
    );
    assert!(!dashboard.apply_resume_text_search_result(old, Err("old failure".into())));
    dashboard.apply_resume_text_search_result(current, Ok(Vec::new()));
    assert!(rows(&dashboard).is_empty());

    dashboard.handle_key(key(KeyCode::Esc));
    assert_eq!(
        rows(&dashboard).len(),
        3,
        "clearing the query restores every live row"
    );
    assert!(
        !dashboard.apply_resume_text_search_result(current, Ok(vec![live_text_hit("live-remote")]))
    );
}

#[test]
fn failed_live_search_keeps_metadata_matches_and_reports_the_failure() {
    let mut dashboard = dashboard_with_live_sessions_in_two_workspaces();
    open_resume_dialog(&mut dashboard, 1, Vec::new());
    let request_id = type_resume_query(&mut dashboard, "mast");
    assert_eq!(titles(&rows(&dashboard)), ["Raise the mast"]);
    dashboard.apply_resume_text_search_result(request_id, Err("index unavailable".into()));
    apply_ready_rows(&mut dashboard, vec![wiki_row("old-session", true)]);
    assert_eq!(titles(&rows(&dashboard)), ["Raise the mast"]);
    let rendered = drawn(&mut dashboard, 180, 40).join("\n");
    assert!(
        rendered.contains("Conversation search failed: index unavailable"),
        "{rendered}"
    );
    switch_to_archive(&mut dashboard);
    assert_eq!(titles(&rows(&dashboard)), ["archived old-session"]);
}

/// I1-16: LAST ACTIVE for a running session is its last activity, not the
/// time its record was last written (often its creation).
// Hard-won: 0841f27b: Finding I1-16 showed the Live tab using record update time instead of the session’s last activity.
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
    open_resume_dialog(&mut dashboard, 1, Vec::new());
    let row = rows(&dashboard)
        .into_iter()
        .find(|row| row.key == ResumeRowKey::Live(session_id.clone()))
        .expect("the session is listed");
    assert_eq!(row.last_activity_ms, active);
}

/// The arrows go on walking the strip after landing on a tab with nothing in
/// it, which is every history tab while all of a dashboard's sessions run.
// Hard-won: c82060f6: Arrow keys stopped switching tabs when the active history list was empty.
#[test]
fn the_arrows_walk_the_strip_across_a_tab_whose_list_is_empty() {
    let mut dashboard = dashboard_with_live_sessions_in_two_workspaces();
    open_resume_dialog(&mut dashboard, 1, Vec::new());
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
// Hard-won: c82060f6: Search-box arrows could not reach the tab strip from the end of a query.
#[test]
fn the_search_arrows_reach_the_strip_only_from_the_end_of_the_query() {
    let mut dashboard = dashboard_with_live_sessions_in_two_workspaces();
    open_resume_dialog(&mut dashboard, 1, Vec::new());
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
// Hard-won: c82060f6: Escape closed the dialog instead of clearing the query and leaving search focus in layers.
#[test]
fn escape_clears_the_query_then_leaves_the_box_then_closes_the_dialog() {
    let mut dashboard = dashboard_with_live_sessions_in_two_workspaces();
    open_resume_dialog(&mut dashboard, 1, Vec::new());
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

/// One match is not "1 matches". The heading counts in the reader's grammar.
// Hard-won: c82060f6: The search heading rendered the singular count as “1 matches”.
#[test]
fn the_search_heading_counts_a_single_match_in_the_singular() {
    let mut dashboard = dashboard_with_live_sessions_in_two_workspaces();
    open_resume_dialog(&mut dashboard, 1, Vec::new());
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

/// Scans arrive one profile at a time; folding one in must not move the
/// Import-tab selection off the native row the user was on.
#[test]
fn an_incremental_scan_update_keeps_the_selected_row() {
    let mut dashboard = DashboardState::new(
        config(),
        state_with(vec![stopped_session()]),
        BTreeMap::new(),
    );
    open_resume_dialog(
        &mut dashboard,
        1,
        vec![codex_profile(vec![native("native-2", "Older", 1)])],
    );
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
    open_resume_dialog(
        &mut dashboard,
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
    open_resume_dialog(
        &mut dashboard,
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
    open_resume_dialog(
        &mut dashboard,
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

/// A search answer can put a row with a transcript under an unmoved
/// selection. The preview pane promises that transcript, so the dialog
/// asks for it without waiting for the selection to move.
// Hard-won: b2dbce5a: A live-terminal finding left the preview stuck on “Loading the archived transcript” after a search answer selected a different row.
#[test]
fn a_search_answer_asks_for_the_newly_selected_rows_transcript() {
    let mut dashboard = DashboardState::new(
        config(),
        state_with(vec![stopped_session()]),
        BTreeMap::new(),
    );
    open_resume_dialog(&mut dashboard, 1, vec![codex_profile(Vec::new())]);
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

#[test]
fn an_empty_tab_keeps_its_answer_while_the_index_syncs() {
    let mut dashboard = DashboardState::new(config(), state_with(Vec::new()), BTreeMap::new());
    open_resume_dialog(&mut dashboard, 1, vec![codex_profile(Vec::new())]);
    replace_search(&mut dashboard, "quokka");
    switch_to_hel(&mut dashboard);
    let (request_id, _) = dashboard.next_wiki_search().unwrap();
    let render = drawn(&mut dashboard, 120, 34).join("\n");
    assert!(render.contains("Searching…"), "{render}");
    assert!(!render.contains("index building"), "{render}");
    for _ in 0..3 {
        dashboard.apply_wiki_search(
            request_id,
            WikiSearchPage {
                rows: Vec::new(),
                status: WikiStatus {
                    state: WikiIndexState::Ready,
                    topping_up: true,
                },
            },
        );
        let render = drawn(&mut dashboard, 120, 34).join("\n");
        assert!(
            render.contains("index syncing, more may arrive"),
            "{render}"
        );
        assert!(!render.contains("Searching…"), "{render}");
        assert!(!dashboard.needs_fast_tick());
    }
    dashboard.apply_wiki_search(request_id, ready_page(Vec::new()));
    let render = drawn(&mut dashboard, 120, 34).join("\n");
    assert!(!render.contains("index syncing"), "{render}");
    assert!(!render.contains("Searching…"), "{render}");
}

#[test]
fn search_errors_obey_query_identity_and_end_the_pending_state() {
    let mut dashboard = DashboardState::new(config(), state_with(Vec::new()), BTreeMap::new());
    open_resume_dialog(&mut dashboard, 1, Vec::new());
    switch_to_archive(&mut dashboard);
    replace_search(&mut dashboard, "quokka");
    let (old, _) = dashboard.next_wiki_search().unwrap();
    let (current, _) = dashboard.next_wiki_search().unwrap();
    assert!(!dashboard.apply_wiki_search_result(old, Err("stale failure".into())));
    assert_eq!(
        resume_dialog(&dashboard.mode).unwrap().wiki_search,
        WikiSearchState::Pending
    );
    assert!(dashboard.apply_wiki_search_result(current, Err("database unavailable".into())));
    assert_eq!(
        resume_dialog(&dashboard.mode).unwrap().wiki_search,
        WikiSearchState::Failed("database unavailable".into())
    );
    let render = drawn(&mut dashboard, 120, 34).join("\n");
    assert!(
        render.contains("Search failed: database unavailable"),
        "{render}"
    );
    assert!(!render.contains("Searching…"), "{render}");
}

#[test]
fn reopening_the_dialog_cannot_accept_the_previous_querys_answer() {
    let mut dashboard = DashboardState::new(config(), state_with(Vec::new()), BTreeMap::new());
    open_resume_dialog(&mut dashboard, 1, Vec::new());
    let (old, _) = dashboard.next_wiki_search().unwrap();
    dashboard.mode = Mode::Dashboard;
    assert_eq!(dashboard.resume_search_request_id(), None);
    open_resume_dialog(&mut dashboard, 2, Vec::new());
    let (current, _) = dashboard.next_wiki_search().unwrap();
    assert_ne!(old, current);
    assert!(!dashboard.apply_wiki_search_result(old, Ok(ready_page(vec![wiki_row("old", true)]))));
    assert!(resume_dialog(&dashboard.mode).unwrap().wiki.is_empty());
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
    open_resume_dialog(&mut dashboard, 1, vec![codex_profile(sessions)]);
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

/// A query's hits are counted on every tab, not only the one on screen, and
/// an empty tab says where the other hits are rather than switching by itself.
// Hard-won: 62ba3c1b: Issue #1161 found stopped sub-agent records could fill the resume list with dozens of rows for one session.
#[test]
fn sub_agents_are_never_offered_for_resume() {
    let owner = stopped_session();
    let managed_child = SessionRecord {
        project: None,
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
            handback_tool: false,
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
    open_resume_dialog(&mut dashboard, 1, Vec::new());
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
    open_resume_dialog(&mut dashboard, 1, vec![codex_profile(Vec::new())]);
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
    open_resume_dialog(&mut dashboard, 1, vec![codex_profile(Vec::new())]);
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
    open_resume_dialog(&mut dashboard, 1, vec![codex_profile(Vec::new())]);
    replace_search(&mut dashboard, "the phrase");
    let (request_id, _) = dashboard.next_wiki_search().expect("a search is asked for");
    let pending = |dashboard: &DashboardState| match &dashboard.mode {
        Mode::ResumeDialog(dialog) => dialog.wiki_search == WikiSearchState::Pending,
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
    assert!(
        dashboard.needs_fast_tick(),
        "Live is still awaiting its independent answer"
    );
    dashboard.apply_resume_text_search_result(request_id, Ok(Vec::new()));
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
/// The pane asks for what it shows: the query's matching passages while a
/// query is active, the briefing when there is none, and a fresh answer for
/// each new query.
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

/// The runtime feed carries only live sessions, so the Mjolnir tab lists what
/// the daemon answers when the dialog opens. Until then neither the stopped
/// sessions nor the import list are shown: nothing yet says which native
/// sessions Mjolnir already holds.
#[test]
fn the_mjolnir_tab_lists_the_daemons_answer_and_resumes_a_session_the_feed_lacks() {
    let mut dashboard = DashboardState::new(config(), state_with(Vec::new()), BTreeMap::new());
    dashboard.show_resume_dialog(
        2,
        vec![codex_profile(vec![
            native("adopted", "Already held", NEWER_THAN_THE_CHECKPOINT),
            native("free", "Importable", NEWER_THAN_THE_CHECKPOINT),
        ])],
    );
    switch_to_hel(&mut dashboard);
    assert!(rows(&dashboard).is_empty());
    switch_to_import(&mut dashboard);
    assert!(rows(&dashboard).is_empty());

    let candidates = || mj_client::daemon::ResumeCandidates {
        candidates: vec![mj_client::daemon::ResumeCandidate::of(
            &stopped_session(),
            &config(),
        )],
        adopted_native_sessions: vec![(HarnessKind::Codex, "adopted".into())],
        ..Default::default()
    };
    assert!(
        !dashboard.apply_resume_candidates(1, Ok(candidates())),
        "an answer for an earlier dialog is dropped"
    );
    assert!(rows(&dashboard).is_empty());
    assert!(dashboard.apply_resume_candidates(2, Ok(candidates())));
    assert_eq!(titles(&rows(&dashboard)), ["Importable"]);
    switch_to_hel(&mut dashboard);
    assert_eq!(titles(&rows(&dashboard)), ["ACP pretty name"]);

    // The rows are previews; the wizard opens on the record fetched for the
    // row picked, and an answer for another row is dropped.
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::LoadResumeRecord {
            session_id: "session-1".into()
        }
    );
    let mut other = stopped_session();
    other.id = "other".into();
    dashboard.apply_resume_record("other", Ok(Some(other)));
    assert!(matches!(dashboard.mode, Mode::ResumeDialog(_)));
    dashboard.apply_resume_record("session-1", Ok(Some(stopped_session())));
    let Mode::Resume(wizard) = &dashboard.mode else {
        panic!("expected the resume wizard");
    };
    assert_eq!(wizard.session_id, "session-1");
    assert!(dashboard.state.sessions.is_empty());
}

#[test]
fn a_search_answer_arriving_under_help_is_kept_when_the_dialog_returns() {
    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    open_resume_dialog(&mut dashboard, 1, Vec::new());
    switch_to_archive(&mut dashboard);
    let request = dashboard.resume_search_request_id().unwrap();
    dashboard.begin_help();
    assert_eq!(dashboard.resume_search_request_id(), Some(request));
    assert!(dashboard.apply_wiki_search_result(
        request,
        Ok(WikiSearchPage {
            status: WikiStatus {
                state: WikiIndexState::Ready,
                topping_up: false
            },
            rows: vec![wiki_row("answered-under-help", true)],
        })
    ));
    assert_eq!(
        dashboard.next_wiki_preview(),
        DashboardAction::LoadArchivedBrief {
            wiki_id: "answered-under-help".into(),
        }
    );
    dashboard.apply_wiki_brief("answered-under-help".into(), "A retained preview".into());
    let Mode::Help(overlay) = std::mem::replace(&mut dashboard.mode, Mode::Dashboard) else {
        panic!("help overlay");
    };
    dashboard.mode = *overlay.return_to;
    let Mode::ResumeDialog(dialog) = &dashboard.mode else {
        panic!("resume dialog");
    };
    assert_eq!(dialog.wiki[0].id, "answered-under-help");
    assert_eq!(
        dialog
            .previews
            .get("answered-under-help")
            .map(String::as_str),
        Some("A retained preview")
    );
    assert!(dialog.preview_pending.is_none());
    assert!(matches!(dialog.wiki_search, WikiSearchState::Answered(_)));
}
