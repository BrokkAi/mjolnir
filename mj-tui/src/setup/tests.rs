use super::*;
use crate::test_support::{
    buffer_lines, cell_column, config, dashboard_with_session, drawn, key, point, running_session,
    stopped_session,
};
use crossterm::event::{KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{Terminal, backend::TestBackend};

#[test]
fn settings_can_add_a_profile_without_file_edits() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_settings_section("profiles", None);
    dashboard.handle_key(key(KeyCode::Char('a')));
    dashboard.handle_paste("muse-account");
    dashboard.handle_key(key(KeyCode::Enter));
    choose(&mut dashboard, "kind");
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    let editor = dialog.editor.as_mut().unwrap();
    let selected = editor
        .choices
        .iter()
        .position(|value| value == "muse")
        .unwrap();
    editor.combo.preview(SetupControl::Choices, selected);
    dialog.prepare();
    dashboard.handle_key(key(KeyCode::Enter));
    choose(&mut dashboard, "home");
    dashboard.handle_paste("/profiles/muse");
    dashboard.handle_key(key(KeyCode::Enter));
    let DashboardAction::SaveSetup { updated, .. } =
        dashboard.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
    else {
        panic!(
            "settings must save: {:?}",
            setup_dialog_mut(&mut dashboard.mode).unwrap().notice
        );
    };
    let saved: Config = serde_json::from_str(&updated).unwrap();
    assert_eq!(
        saved.profiles["muse-account"].kind,
        mj_core::config::HarnessKind::Muse
    );
}

#[test]
fn subagent_profile_choices_are_checkboxes_and_false_entries_are_not_persisted() {
    let config = config();
    let dialog = SetupDialog::new(&config);
    let choices = dialog.draft["subagents"]["eligible_profiles"]
        .as_object()
        .unwrap();
    assert_eq!(choices.len(), config.profiles.len());
    assert!(choices.values().all(|value| value == &Value::Bool(false)));

    let mut draft = dialog.draft;
    let profile_id = config.profiles.keys().next().unwrap();
    draft["subagents"]["eligible_profiles"][profile_id] = Value::Bool(true);
    let parsed = config_from_draft(draft).unwrap();
    assert_eq!(parsed.subagents.eligible_profiles.len(), 1);
    assert_eq!(
        parsed.subagents.eligible_profiles.get(profile_id),
        Some(&true)
    );
}

fn choose(dashboard: &mut DashboardState, name: &str) {
    let Mode::Setup(dialog) = &mut dashboard.mode else {
        panic!("settings");
    };
    dialog.selected = dialog.keys().iter().position(|key| key == name).unwrap();
    dialog.form.get_mut().focus(SetupControl::List);
    dialog.prepare();
    dashboard.handle_key(key(KeyCode::Enter));
}

fn activate(dashboard: &mut DashboardState, control: SetupControl) {
    let Mode::Setup(dialog) = &mut dashboard.mode else {
        panic!("settings");
    };
    dialog.form.get_mut().focus(control);
    dashboard.handle_key(key(KeyCode::Enter));
}

fn choose_light_theme(dashboard: &mut DashboardState) {
    dashboard.handle_key(key(KeyCode::F(7)));
    choose(dashboard, "interface");
    choose(dashboard, "theme");
    dashboard.handle_key(key(KeyCode::Down));
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Backspace));
}

fn assert_rendered_theme(dashboard: &mut DashboardState, selected: theme::UiTheme) {
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|frame| crate::render::render(frame, dashboard))
        .unwrap();
    let colors = theme::palette_for(selected);
    let buffer = terminal.backend().buffer();
    let surface = if dashboard.modal_open() {
        colors.surface_raised
    } else {
        colors.surface
    };
    assert!(
        buffer
            .content
            .iter()
            .any(|cell| { cell.bg == surface && cell.fg == colors.text && cell.symbol() != " " })
    );
    assert!(buffer.content.iter().any(|cell| cell.fg == colors.accent));
}

#[test]
fn account_path_apply_expands_home_before_config_and_quota_use() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    choose(&mut dashboard, "profiles");
    choose(&mut dashboard, "codex-1");
    choose(&mut dashboard, "home");
    let Mode::Setup(dialog) = &mut dashboard.mode else {
        panic!("settings");
    };
    let editor = dialog.editor.as_mut().unwrap();
    assert!(matches!(editor.input, EditorInput::Path(_)));
    editor.input.clear();
    dashboard.handle_paste("~/.codex4");
    dashboard.handle_key(key(KeyCode::Enter));
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings");
    };
    assert!(dialog.editor.is_none(), "{:?}", dialog.notice);
    let config: Config = config_from_draft(dialog.draft.clone()).unwrap();
    let expected = mj_core::path_input::expand_local(std::path::Path::new("~/.codex4")).unwrap();
    let profile = &config.profiles["codex-1"];
    assert_eq!(profile.home, expected);
    let mut environment = profile.environment.clone();
    profile
        .kind
        .configure_home_environment(&profile.home, &mut environment);
    assert_eq!(environment["CODEX_HOME"], expected.to_string_lossy());
}

#[test]
fn remote_path_apply_preserves_failed_and_newer_drafts() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.config.targets.insert(
        "remote-path".into(),
        serde_json::from_value(
            json!({"kind":"ssh-bare","host":"builder","permissions":"guardian"}),
        )
        .unwrap(),
    );
    dashboard.begin_setup();
    choose(&mut dashboard, "targets");
    choose(&mut dashboard, "remote-path");
    choose(&mut dashboard, "workspace_prefix");
    let Mode::Setup(dialog) = &mut dashboard.mode else {
        panic!("settings");
    };
    dialog.editor.as_mut().unwrap().input.set_value("~/work");
    let DashboardAction::ResolveSetupPath {
        generation,
        draft,
        path,
        value,
        ..
    } = dashboard.handle_key(key(KeyCode::Enter))
    else {
        panic!("resolve path");
    };
    dashboard.setup_path_resolved(
        generation,
        &draft,
        &path,
        &value,
        Err("SSH unavailable".into()),
    );
    let Mode::Setup(dialog) = &mut dashboard.mode else {
        panic!("settings");
    };
    assert_eq!(dialog.editor.as_ref().unwrap().input.value(), "~/work");
    assert_eq!(dialog.notice.as_deref(), Some("SSH unavailable"));
    dialog.editor.as_mut().unwrap().input.set_value("~/newer");
    dashboard.setup_path_resolved(generation, &draft, &path, &value, Ok("/remote/work".into()));
    let Mode::Setup(dialog) = &mut dashboard.mode else {
        panic!("settings");
    };
    assert_eq!(dialog.editor.as_ref().unwrap().input.value(), "~/newer");
    dialog.editor.as_mut().unwrap().input.set_value(&value);
    dashboard.setup_path_resolved(generation, &draft, &path, &value, Ok("/remote/work".into()));
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings");
    };
    assert_eq!(
        dialog.draft["targets"]["remote-path"]["workspace_prefix"],
        "/remote/work"
    );
}

#[test]
fn setup_root_renders_virtual_interface_without_physical_interface_rows() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .unwrap();
    let text = buffer_lines(terminal.backend().buffer()).join("\n");
    assert!(text.contains("Interface"), "{text}");
    assert!(text.contains("Advanced"), "{text}");
    for section in [
        "Agent Profiles",
        "Machines and Runtimes",
        "Projects",
        "Code Review",
        "Web Access",
    ] {
        assert!(text.contains(section), "missing {section:?} in {text}");
    }
    assert!(!text.contains("Session sidebar position"), "{text}");
    assert!(!text.contains("Activity animation"), "{text}");
    assert!(!text.contains("Theme"), "{text}");
}

#[test]
fn interface_choice_commits_to_the_existing_root_storage_path() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    choose(&mut dashboard, "interface");
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings");
    };
    assert_eq!(dialog.keys(), ["sessions_side", "spinner", "theme"]);
    assert_eq!(dialog.draft["theme"], "midnight");

    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .unwrap();
    let interface = buffer_lines(terminal.backend().buffer()).join("\n");
    assert_eq!(interface.matches('▾').count(), 3, "{interface}");

    choose(&mut dashboard, "theme");
    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .unwrap();
    let popup = buffer_lines(terminal.backend().buffer()).join("\n");
    assert!(popup.contains("values"), "{popup}");
    dashboard.handle_key(key(KeyCode::Down));
    dashboard.handle_key(key(KeyCode::Tab));
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings");
    };
    assert!(dialog.editor.is_none());
    assert_eq!(dialog.path, ["interface"]);
    assert_eq!(dialog.draft["theme"], "light");

    dashboard.handle_key(key(KeyCode::Backspace));
    choose(&mut dashboard, "phone");
    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .unwrap();
    let free_text = buffer_lines(terminal.backend().buffer()).join("\n");
    assert!(!free_text.contains('▾'), "{free_text}");
}

#[test]
fn choice_popup_escape_preserves_the_draft_and_background_click_is_inert() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    choose(&mut dashboard, "interface");
    choose(&mut dashboard, "theme");
    dashboard.handle_key(key(KeyCode::Down));
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .unwrap();

    // A click away from the popup must not activate the inert mirror of the
    // page's stacked controls or commit the pending choice.
    let (row, column) = buffer_lines(terminal.backend().buffer())
        .iter()
        .enumerate()
        .find_map(|(row, line)| line.find("Save and Close").map(|column| (row, column)))
        .expect("mirrored Save row");
    let background_button = (column as u16 + 1, row as u16);
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        dashboard.handle_event_result(Event::Mouse(MouseEvent {
            kind,
            column: background_button.0,
            row: background_button.1,
            modifiers: KeyModifiers::NONE,
        }));
    }
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings");
    };
    assert!(dialog.editor.is_some());
    assert_eq!(dialog.draft["theme"], "midnight");
    dashboard.handle_key(key(KeyCode::Esc));
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings");
    };
    assert!(dialog.editor.is_none(), "escape closes the popup");
    assert_eq!(dialog.draft["theme"], "midnight");

    // Reopen it for the pointer-commit part of the behavior.
    choose(&mut dashboard, "theme");
    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .unwrap();

    // Find one popup content cell from the rendered form and click it.
    let (row, column) = buffer_lines(terminal.backend().buffer())
        .iter()
        .enumerate()
        .find_map(|(row, line)| line.find("Light").map(|column| (row, column)))
        .expect("Light popup row");
    let point = (column as u16, row as u16);
    assert!(
        setup_dialog_mut(&mut dashboard.mode)
            .is_some_and(|dialog| dialog.form.borrow().contains(point.0, point.1))
    );
    dashboard.handle_event_result(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: point.0,
        row: point.1,
        modifiers: KeyModifiers::NONE,
    }));
    dashboard.handle_event_result(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Left),
        column: point.0,
        row: point.1,
        modifiers: KeyModifiers::NONE,
    }));
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings");
    };
    assert!(dialog.editor.is_none(), "click commits the popup choice");
    assert_eq!(dialog.draft["theme"], "light");
}

#[test]
fn setup_keeps_one_content_size_across_pages_editors_and_code_review() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    let area = Rect::new(0, 0, 100, 30);
    let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
    let mut expected = None;
    let mut assert_rect = |dashboard: &mut DashboardState| {
        terminal
            .draw(|frame| crate::render::render(frame, dashboard))
            .unwrap();
        let rect = dashboard
            .frame_surfaces()
            .surface(mj_chat::selection::SurfaceId::ModalBody)
            .expect("rendered Settings modal surface")
            .rect;
        assert!(
            rect.width < mj_chat::modal::modal_area(area).width,
            "settings should be compact: {rect:?}"
        );
        assert_eq!(expected.get_or_insert(rect), &rect);
    };

    assert_rect(&mut dashboard); // root
    choose(&mut dashboard, "interface");
    assert_rect(&mut dashboard);
    choose(&mut dashboard, "theme");
    assert_rect(&mut dashboard); // inline choice popup
    dashboard.handle_key(key(KeyCode::Esc));
    dashboard.handle_key(key(KeyCode::Backspace));
    choose(&mut dashboard, "advanced");
    assert_rect(&mut dashboard);
    dashboard.handle_key(key(KeyCode::Backspace));
    choose(&mut dashboard, "phone");
    choose(&mut dashboard, "bind");
    assert_rect(&mut dashboard); // free text editor
    activate(&mut dashboard, SetupControl::Back);
    dashboard.handle_key(key(KeyCode::Backspace));
    choose(&mut dashboard, "review");
    assert_rect(&mut dashboard); // Code Review child
}

#[test]
fn theme_selection_applies_after_save_and_is_restored_when_setup_reopens() {
    let mut dashboard = dashboard_with_session(stopped_session());
    choose_light_theme(&mut dashboard);
    assert_eq!(dashboard.config.theme, theme::UiTheme::Midnight);
    assert_rendered_theme(&mut dashboard, theme::UiTheme::Midnight);
    let action = dashboard.handle_key(crossterm::event::KeyEvent::new(
        KeyCode::Char('s'),
        KeyModifiers::CONTROL,
    ));
    let DashboardAction::SaveSetup {
        generation,
        updated,
        ..
    } = action
    else {
        panic!("{action:?}");
    };
    let saved: Config = serde_json::from_str(&updated).unwrap();
    assert_eq!(saved.theme, theme::UiTheme::Light);
    assert_rendered_theme(&mut dashboard, theme::UiTheme::Midnight);
    dashboard.setup_saved(generation, Ok(saved));
    assert!(!dashboard.modal_open());
    assert_rendered_theme(&mut dashboard, theme::UiTheme::Light);

    dashboard.begin_setup();
    assert_rendered_theme(&mut dashboard, theme::UiTheme::Light);
    choose(&mut dashboard, "interface");
    choose(&mut dashboard, "theme");
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    let editor = dialog.editor.as_ref().unwrap();
    let selected = editor
        .combo
        .selection(SetupControl::Choices, editor.selected);
    assert_eq!(editor.choices[selected], "light");
}

#[test]
fn cancelling_or_failing_to_save_a_theme_keeps_the_active_colors() {
    let mut dashboard = dashboard_with_session(stopped_session());
    let original = dashboard.config.clone();
    choose_light_theme(&mut dashboard);
    dashboard.handle_key(key(KeyCode::Esc));
    assert!(dashboard.modal_open());
    dashboard.handle_key(key(KeyCode::Right));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert!(!dashboard.modal_open());
    assert_eq!(dashboard.config, original);
    assert_rendered_theme(&mut dashboard, theme::UiTheme::Midnight);

    choose_light_theme(&mut dashboard);
    let action = dashboard.handle_key(crossterm::event::KeyEvent::new(
        KeyCode::Char('s'),
        KeyModifiers::CONTROL,
    ));
    let DashboardAction::SaveSetup { generation, .. } = action else {
        panic!("{action:?}");
    };
    dashboard.setup_saved(generation, Err("disk full".into()));
    assert_eq!(dashboard.config, original);
    assert_rendered_theme(&mut dashboard, theme::UiTheme::Midnight);
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert_eq!(dialog.draft["theme"], "light");
    assert!(dialog.notice.as_ref().unwrap().contains("disk full"));
}

#[test]
fn setup_root_uses_modal_title_once_and_nested_pages_keep_breadcrumb() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .unwrap();
    let root = buffer_lines(terminal.backend().buffer()).join("\n");
    assert_eq!(root.matches("Settings").count(), 1, "{root}");

    choose(&mut dashboard, "phone");
    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .unwrap();
    let nested = buffer_lines(terminal.backend().buffer()).join("\n");
    assert!(nested.contains("Settings › Web Access"), "{nested}");
}

#[test]
fn disabling_a_profile_clears_references_and_reports_the_cleanup() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.config.review.enabled = true;
    dashboard.config.review.profile = Some("codex-1".into());
    dashboard.config.review.model = Some("review-model".into());
    dashboard.config.review.effort = Some("high".into());
    dashboard.begin_setup();
    choose(&mut dashboard, "profiles");
    choose(&mut dashboard, "codex-1");

    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .unwrap();
    let enabled = buffer_lines(terminal.backend().buffer()).join("\n");
    assert!(enabled.contains('☑'), "{enabled}");

    choose(&mut dashboard, "enabled");
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings");
    };
    assert_eq!(dialog.draft["profiles"]["codex-1"]["enabled"], false);
    assert!(dialog.draft["review"]["profile"].is_null());
    assert_eq!(dialog.draft["review"]["enabled"], false);
    assert_eq!(dialog.draft["review"]["model"], "review-model");
    assert_eq!(dialog.draft["review"]["effort"], "high");
    let notice = dialog.notice.as_deref().unwrap();
    assert!(notice.contains("Code Review"), "{notice}");

    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .unwrap();
    let disabled = buffer_lines(terminal.backend().buffer()).join("\n");
    assert!(disabled.contains('☐'), "{disabled}");
}

#[test]
fn detection_adds_conflicting_installations_to_the_draft_without_losing_settings() {
    let mut dashboard = dashboard_with_session(stopped_session());
    let original = dashboard.config.clone();
    dashboard.begin_setup();
    let generation = setup_dialog_mut(&mut dashboard.mode).unwrap().generation;
    let mut discovered = original.clone();
    discovered.profiles.get_mut("codex-1").unwrap().home = "/profiles/new-codex".into();
    discovered
        .targets
        .insert("podman".into(), mj_core::config::TargetTemplate::LocalBare);
    discovered.bundles.get_mut("hel").unwrap().repositories[0].github =
        Some("owner/new-repository".into());
    for _ in 0..2 {
        for scope in [DetectScope::Profiles, DetectScope::Runtimes] {
            dashboard.setup_discovered(generation, Ok(detection(scope, discovered.clone())));
        }
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        let draft: Config = config_from_draft(dialog.draft.clone()).unwrap();
        assert_eq!(draft.profiles["codex-1"], original.profiles["codex-1"]);
        assert_eq!(
            draft.profiles["codex-1-2"].home,
            std::path::PathBuf::from("/profiles/new-codex")
        );
        assert_eq!(draft.targets["podman"], original.targets["podman"]);
        assert!(matches!(
            draft.targets["podman-2"],
            mj_core::config::TargetTemplate::LocalBare
        ));
        // Neither detection touches projects, so the draft keeps only the
        // bundles the user already had.
        assert_eq!(draft.bundles, original.bundles);
        assert_eq!(draft.profiles.len(), original.profiles.len() + 1);
        assert_eq!(draft.targets.len(), original.targets.len() + 1);
    }
    assert_eq!(dashboard.config, original, "discovery only edits the draft");
}

fn detection(scope: DetectScope, config: Config) -> crate::setup::SetupDetection {
    crate::setup::SetupDetection {
        scope,
        config,
        rejected_runtimes: Vec::new(),
    }
}

#[test]
fn detecting_runtimes_names_what_it_added_and_what_it_skipped() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    let generation = setup_dialog_mut(&mut dashboard.mode).unwrap().generation;
    let mut discovered = Config::default();
    discovered.targets.insert(
        "localhost".into(),
        mj_core::config::TargetTemplate::LocalBare,
    );
    dashboard.setup_discovered(
        generation,
        Ok(crate::setup::SetupDetection {
            scope: DetectScope::Runtimes,
            config: discovered,
            rejected_runtimes: vec![crate::setup::RejectedRuntime {
                label: "Docker".into(),
                detail: "the Docker daemon is not running".into(),
                remediation: Some("Start Docker Desktop".into()),
            }],
        }),
    );
    let notice = setup_dialog_mut(&mut dashboard.mode)
        .unwrap()
        .notice
        .clone()
        .expect("detection reports what it did");
    assert!(notice.contains("localhost"), "{notice}");
    assert!(
        notice.contains("Skipped Docker: the Docker daemon is not running."),
        "{notice}"
    );
    assert!(notice.contains("Start Docker Desktop"), "{notice}");

    // A second run finds nothing new and says so rather than claiming an
    // addition.
    let mut again = Config::default();
    again.targets.insert(
        "localhost".into(),
        mj_core::config::TargetTemplate::LocalBare,
    );
    dashboard.setup_discovered(generation, Ok(detection(DetectScope::Runtimes, again)));
    let notice = setup_dialog_mut(&mut dashboard.mode)
        .unwrap()
        .notice
        .clone()
        .expect("detection reports what it did");
    assert!(
        notice.starts_with("No usable runtime was found"),
        "{notice}"
    );
}

#[test]
fn results_from_a_closed_setup_do_not_change_the_new_draft() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    let old = setup_dialog_mut(&mut dashboard.mode).unwrap().generation;
    dashboard.cancel_modal();
    dashboard.begin_setup();
    let new = setup_dialog_mut(&mut dashboard.mode).unwrap().generation;
    let original = setup_dialog_mut(&mut dashboard.mode).unwrap().draft.clone();
    let mut detected = dashboard.config.clone();
    detected.targets.insert(
        "stale-discovery".into(),
        mj_core::config::TargetTemplate::LocalBare,
    );
    dashboard.setup_discovered(old, Ok(detection(DetectScope::Runtimes, detected)));
    dashboard.setup_saved(old, Ok(dashboard.config.clone()));
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert_eq!(dialog.generation, new);
    assert_eq!(dialog.draft, original);
}

#[test]
fn setup_does_not_offer_automatic_session_settings() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert!(!dialog.keys().iter().any(|key| key == "startup"));
    assert!(dialog.draft.get("startup").is_none());
}

#[test]
fn stopped_session_visibility_is_only_editable_under_advanced() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    assert!(
        !setup_dialog_mut(&mut dashboard.mode)
            .unwrap()
            .keys()
            .iter()
            .any(|key| key == "show_stopped_sessions")
    );

    choose(&mut dashboard, "advanced");
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert_eq!(
        dialog.keys(),
        ["detailed_activity_clocks", "show_stopped_sessions"]
    );
    assert_eq!(dialog.draft["advanced"]["show_stopped_sessions"], false);

    choose(&mut dashboard, "show_stopped_sessions");
    assert_eq!(
        setup_dialog_mut(&mut dashboard.mode).unwrap().draft["advanced"]["show_stopped_sessions"],
        true
    );
    let action = dashboard.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
    let DashboardAction::SaveSetup { updated, .. } = action else {
        panic!("expected Settings save, got {action:?}")
    };
    let saved: Config = serde_json::from_str(&updated).unwrap();
    assert!(saved.advanced.show_stopped_sessions);
    assert!(!saved.show_stopped_sessions);
}

#[test]
fn setup_adds_a_remote_runtime_and_reports_invalid_fields_without_losing_the_draft() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    choose(&mut dashboard, "targets");
    dashboard.handle_key(key(KeyCode::Char('a')));
    dashboard.handle_paste("builder");
    dashboard.handle_key(key(KeyCode::Enter));
    choose(&mut dashboard, "kind");
    let Mode::Setup(dialog) = &mut dashboard.mode else {
        panic!("settings");
    };
    let editor = dialog.editor.as_mut().unwrap();
    let selected = editor
        .choices
        .iter()
        .position(|value| value == "ssh-docker")
        .unwrap();
    assert!(editor.combo.preview(SetupControl::Choices, selected));
    dialog.prepare();
    dashboard.handle_key(key(KeyCode::Enter));
    let action = dashboard.handle_key(crossterm::event::KeyEvent::new(
        KeyCode::Char('s'),
        KeyModifiers::CONTROL,
    ));
    assert_eq!(action, DashboardAction::None);
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings");
    };
    assert!(dialog.notice.as_ref().unwrap().contains("SSH host"));
    choose(&mut dashboard, "host");
    dashboard.handle_paste("builder.example.test");
    dashboard.handle_key(key(KeyCode::Enter));
    let action = dashboard.handle_key(crossterm::event::KeyEvent::new(
        KeyCode::Char('s'),
        KeyModifiers::CONTROL,
    ));
    let DashboardAction::SaveSetup { updated, .. } = action else {
        panic!("{action:?}");
    };
    let saved: Config = serde_json::from_str(&updated).unwrap();
    assert!(
        matches!(&saved.targets["builder"], mj_core::config::TargetTemplate::SshDocker { ssh, .. } if ssh.host == "builder.example.test")
    );
    let generation = setup_dialog_mut(&mut dashboard.mode).unwrap().generation;
    dashboard.setup_saved(generation, Err("disk full".into()));
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings");
    };
    assert!(!dialog.saving);
    assert_eq!(
        dialog.draft["targets"]["builder"]["host"],
        "builder.example.test"
    );
    assert!(dialog.notice.as_ref().unwrap().contains("disk full"));
}

fn action_labels(dashboard: &DashboardState) -> Vec<&'static str> {
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings");
    };
    dialog
        .actions()
        .into_iter()
        .map(|(_, label, _)| label)
        .collect()
}

/// The footer row's labels and the right-hand column's labels, in that order.
fn split_action_labels(dashboard: &DashboardState) -> (Vec<&'static str>, Vec<&'static str>) {
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings");
    };
    let labels = |actions: Vec<(SetupControl, &'static str, bool)>| {
        actions
            .into_iter()
            .map(|(_, label, _)| label)
            .collect::<Vec<_>>()
    };
    (
        labels(dialog.footer_actions()),
        labels(dialog.page_actions()),
    )
}

#[test]
fn setup_actions_offer_only_the_controls_that_apply_to_the_page() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    assert_eq!(
        action_labels(&dashboard),
        ["Save and Close"],
        "root: no Back, no item actions, and nothing to detect"
    );
    assert_eq!(
        split_action_labels(&dashboard),
        (vec!["Save and Close"], Vec::new()),
        "root: the footer commit alone, and no column beside the body"
    );
    choose(&mut dashboard, "targets");
    assert_eq!(
        action_labels(&dashboard),
        ["Back", "Add", "Remove", "Detect runtimes", "Save and Close"],
        "machines: item actions and the runtime detection only this page offers"
    );
    assert_eq!(
        split_action_labels(&dashboard),
        (
            vec!["Back", "Save and Close"],
            vec!["Add", "Remove", "Detect runtimes"]
        ),
        "machines: navigation in the footer, page actions in the column"
    );
    dashboard.handle_key(key(KeyCode::Backspace));
    choose(&mut dashboard, "bundles");
    assert_eq!(
        action_labels(&dashboard),
        ["Back", "Create", "Remove", "Save and Close"],
        "projects: the collection button names what it makes"
    );
    dashboard.handle_key(key(KeyCode::Backspace));
    choose(&mut dashboard, "profiles");
    assert_eq!(
        action_labels(&dashboard),
        ["Back", "Add", "Remove", "Detect profiles", "Save and Close"],
        "profiles: item actions and the profile detection only this page offers"
    );
    dashboard.handle_key(key(KeyCode::Backspace));
    choose(&mut dashboard, "phone");
    assert_eq!(
        action_labels(&dashboard),
        ["Back", "Save and Close"],
        "leaf outside the detected sections: no Add, Remove, or Detect"
    );
    choose(&mut dashboard, "bind");
    assert_eq!(
        action_labels(&dashboard),
        ["Back", "Use default", "Apply"],
        "text editor"
    );
    assert_eq!(
        split_action_labels(&dashboard),
        (vec!["Back"], vec!["Use default", "Apply"]),
        "text editor: only Back in the footer, since there is nothing to save yet"
    );
    activate(&mut dashboard, SetupControl::Back);
    dashboard.handle_key(key(KeyCode::Backspace));
    choose(&mut dashboard, "targets");
    activate(&mut dashboard, SetupControl::Add);
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings");
    };
    assert!(dialog.editor.as_ref().is_some_and(|editor| editor.adding));
    assert_eq!(
        action_labels(&dashboard),
        ["Back", "Apply"],
        "a new name has no default to restore"
    );
}

#[test]
fn the_root_page_takes_the_full_width_and_shows_only_the_footer_action() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    let mut terminal = Terminal::new(TestBackend::new(140, 42)).unwrap();
    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .unwrap();
    let lines = buffer_lines(terminal.backend().buffer());
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings");
    };
    // The root row is `{name:<32}  {summary}`, the widest line this config
    // renders. The root page has no actions of its own, so the body keeps the
    // whole width and nothing may clip that row.
    let summary = value_summary(
        &[],
        "targets",
        &dialog.draft["targets"],
        &dialog.draft,
        None,
    );
    let row = format!("{:<32}  {summary}", schema::label("targets"));
    let line = lines
        .iter()
        .find(|line| line.contains("Machines and Runtimes"))
        .unwrap_or_else(|| panic!("missing the machines row in\n{}", lines.join("\n")));
    assert!(line.contains(&row), "the page row was clipped: {line:?}");
    let text = lines.join("\n");
    // Save and Close is the only action the root page offers, and it sits in
    // the footer row rather than a column beside the body.
    for absent in ["  Back  ", "  Add  ", "  Remove  ", "  Detect runtimes  "] {
        assert!(
            !text.contains(absent),
            "{absent:?} on the root page:\n{text}"
        );
    }
    let (save_column, save_row) = point(&lines, "  Save and Close  ");
    assert_eq!(
        save_column,
        cell_column(line, &row),
        "the footer is not packed to the body's left edge:\n{text}"
    );
    let (_, body_row) = point(&lines, &row);
    assert!(
        save_row > body_row,
        "the footer is not below the body:\n{text}"
    );
}

#[test]
fn a_collection_page_stacks_its_own_actions_above_the_footer_row() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.begin_setup();
    choose(&mut dashboard, "targets");
    let lines = drawn(&mut dashboard, 100, 30);
    let text = lines.join("\n");
    // Back and the commit share one left-packed row at the dialog's bottom.
    let (back_column, back_row) = point(&lines, "  Back  ");
    let (save_column, save_row) = point(&lines, "  Save and Close  ");
    assert_eq!(back_row, save_row, "the footer buttons split rows:\n{text}");
    assert!(
        back_column < save_column,
        "Back does not lead the footer row:\n{text}"
    );
    let body_column = point(&lines, "podman").0;
    assert_eq!(
        back_column, body_column,
        "the footer is not packed to the body's left edge:\n{text}"
    );
    // The page's own actions stay stacked at the dialog's right edge, above
    // the footer row.
    let stacked =
        ["  Add  ", "  Remove  ", "  Detect runtimes  "].map(|label| point(&lines, label));
    for (column, row) in stacked {
        assert_eq!(
            column, stacked[0].0,
            "the page actions do not share a column:\n{text}"
        );
        assert!(
            row < back_row,
            "a page action sits on or below the footer row:\n{text}"
        );
        assert!(
            column > save_column,
            "a page action is not at the dialog's right edge:\n{text}"
        );
    }
    assert!(
        stacked[0].1 + 1 == stacked[1].1 && stacked[1].1 + 1 == stacked[2].1,
        "the page actions are not stacked in order: {stacked:?}\n{text}"
    );
    // The widest button ends against the inner margin and the modal border.
    let detect = &lines[usize::from(stacked[2].1)];
    assert!(
        detect.contains("Detect runtimes   │"),
        "the column is not packed against the right edge: {detect:?}"
    );
}

#[test]
fn backspace_at_the_root_with_a_dirty_draft_asks_before_discarding() {
    let mut dashboard = dashboard_with_session(stopped_session());
    let original = dashboard.config.clone();
    dashboard.begin_setup();
    choose(&mut dashboard, "phone");
    choose(&mut dashboard, "enabled");
    dashboard.handle_key(key(KeyCode::Backspace));
    dashboard.handle_key(key(KeyCode::Backspace));
    let Mode::Confirm(_) = &dashboard.mode else {
        panic!(
            "dirty settings must confirm before closing: {:?}",
            dashboard.modal_open()
        );
    };
    // Esc keeps editing with the draft intact.
    dashboard.handle_key(key(KeyCode::Esc));
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings restored");
    };
    assert!(dialog.is_dirty());
    assert_eq!(dashboard.config, original);
}

#[test]
fn cancelling_setup_preserves_configuration_and_render_keeps_controls_visible() {
    let mut dashboard = dashboard_with_session(stopped_session());
    let original = dashboard.config.clone();
    for (width, height) in [(80, 18), (100, 30), (140, 42)] {
        dashboard.begin_setup();
        choose(&mut dashboard, "phone");
        choose(&mut dashboard, "enabled");
        dashboard.handle_key(key(KeyCode::Backspace));
        choose(&mut dashboard, "targets");
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, &mut dashboard))
            .unwrap();
        let lines = buffer_lines(terminal.backend().buffer());
        let text = lines.join("\n");
        assert!(text.contains("Settings › Machines and Runtimes"), "{text}");
        // Every page action keeps its own row in one column at the dialog's
        // right edge, on top of each other rather than spread along a row.
        let labels = ["Add", "Remove", "Detect runtimes"];
        let width = labels
            .iter()
            .map(|label| label.len())
            .max()
            .expect("labels");
        let mut rows = Vec::new();
        for label in labels {
            // The button's padding separates it from prose that happens to
            // use the same word, such as the page's help line.
            let padded = format!("  {label}  ");
            let (row, line) = lines
                .iter()
                .enumerate()
                .find(|(_, line)| line.contains(&padded))
                .unwrap_or_else(|| panic!("missing {label:?} in\n{text}"));
            // Every button is as wide as the longest of them, so a shorter
            // label is followed by its share of that width, the button's
            // padding, the inner margin, and then the modal border.
            let after = &line[line.find(&padded).unwrap() + 2 + label.len()..];
            let gap = format!("{}│", " ".repeat(3 + width - label.len()));
            assert!(
                after.starts_with(&gap),
                "{label} is not packed against the dialog's right edge: {line}"
            );
            rows.push((row, cell_column(line, &padded) + 2, label));
        }
        assert!(
            rows.windows(2).all(|pair| pair[0].0 < pair[1].0),
            "the buttons are not stacked in order: {rows:?}\n{text}"
        );
        let (_, first_column, _) = rows[0];
        assert!(
            rows.iter().all(|(_, column, _)| *column == first_column),
            "the stacked buttons do not share a column: {rows:?}\n{text}"
        );
        assert!(
            rows.windows(2).all(|pair| pair[0].0 + 1 == pair[1].0),
            "the stacked buttons leave gaps between them: {rows:?}\n{text}"
        );
        // Back and the commit share the footer row below the column.
        let (back_column, back_row) = point(&lines, "  Back  ");
        let (save_column, save_row) = point(&lines, "  Save and Close  ");
        assert_eq!(back_row, save_row, "{text}");
        assert!(back_column < save_column, "{text}");
        assert!(
            rows.iter().all(|(row, _, _)| (*row as u16) < back_row),
            "the column overlaps the footer row: {rows:?}\n{text}"
        );
        assert!(!text.contains("Cancel"), "{text}");
        // Backspace from the root dismisses through the dirty guard.
        dashboard.handle_key(key(KeyCode::Backspace));
        dashboard.handle_key(key(KeyCode::Backspace));
        assert!(matches!(dashboard.mode, Mode::Confirm(_)), "{text}");
        dashboard.handle_key(key(KeyCode::Right));
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::None
        );
        assert!(!dashboard.modal_open());
        assert_eq!(dashboard.config, original);
    }
}

#[test]
fn review_changes_stay_in_setup_draft_until_save_and_cancel_discards_them() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    choose(&mut dashboard, "review");
    dashboard.handle_key(key(KeyCode::Char(' ')));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Esc)),
        DashboardAction::None
    );
    assert!(dashboard.dialog_confirmation_open());
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Esc)),
        DashboardAction::None
    );
    assert!(matches!(dashboard.mode, Mode::Setup(_)));
    assert!(!dashboard.config.review.enabled);
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings remains open after leaving review")
    };
    assert!(dialog.draft["review"]["enabled"].as_bool().unwrap());
    dashboard.handle_key(key(KeyCode::Esc));
    assert!(dashboard.modal_open());
    dashboard.handle_key(key(KeyCode::Right));
    dashboard.handle_key(key(KeyCode::Enter));
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("discard returns to settings")
    };
    assert!(dialog.review_editor.is_none());
    assert!(!dialog.draft["review"]["enabled"].as_bool().unwrap_or(false));
    dashboard.handle_key(key(KeyCode::Esc));
    assert!(!dashboard.modal_open());
    assert!(!dashboard.config.review.enabled);

    dashboard.begin_setup();
    choose(&mut dashboard, "review");
    dashboard.handle_key(key(KeyCode::Char(' ')));
    dashboard.handle_key(key(KeyCode::Tab));
    dashboard.handle_key(key(KeyCode::Tab));
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Down));
    dashboard.handle_key(key(KeyCode::Enter));
    let action = dashboard.handle_key(crossterm::event::KeyEvent::new(
        KeyCode::Char('s'),
        KeyModifiers::CONTROL,
    ));
    assert!(matches!(action, DashboardAction::SaveSetup { .. }));
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings save remains pending")
    };
    let generation = dialog.generation;
    let updated: Config = serde_json::from_str(match &action {
        DashboardAction::SaveSetup { updated, .. } => updated,
        _ => unreachable!(),
    })
    .unwrap();
    assert!(updated.review.enabled);
    dashboard.setup_saved(generation, Ok(updated));
    assert!(!dashboard.modal_open());
    assert!(dashboard.config.review.enabled);
}

#[test]
fn unsaved_account_edits_block_review_cache_and_refresh_until_setup_is_saved() {
    use crate::review_settings::{ReviewSettingsChoices, ReviewSettingsFocus};

    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.config.review.profile = Some("codex-1".into());
    dashboard.config.review.enabled = true;
    dashboard
        .review_settings_choices
        .insert(("codex-1".into(), None), ReviewSettingsChoices::default());
    dashboard.begin_setup();
    choose(&mut dashboard, "profiles");
    choose(&mut dashboard, "codex-1");
    choose(&mut dashboard, "home");
    dashboard.handle_paste("-changed");
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Backspace));
    dashboard.handle_key(key(KeyCode::Backspace));
    choose(&mut dashboard, "review");
    let setup = setup_dialog_mut(&mut dashboard.mode).unwrap();
    let review = setup.review_editor.as_mut().unwrap();
    assert!(!review.probing);
    assert!(
        !review.model_choices_discovered,
        "old account cache must not apply"
    );
    assert!(
        review
            .discovery_error
            .as_deref()
            .unwrap()
            .contains("Save account changes")
    );
    review.form.get_mut().focus(ReviewSettingsFocus::Refresh);
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::CancelReviewSettingsDiscovery
    );
    let setup = setup_dialog_mut(&mut dashboard.mode).unwrap();
    let review = setup.review_editor.as_mut().unwrap();
    review.form.get_mut().focus(ReviewSettingsFocus::Profile);
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Down)),
        DashboardAction::None
    );
    assert!(matches!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::DiscoverReviewSettings { profile_id, .. } if profile_id == "codex-2"
    ));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Up)),
        DashboardAction::None
    );
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::CancelReviewSettingsDiscovery
    );
    let DashboardAction::SaveSetup { updated, .. } =
        dashboard.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
    else {
        panic!("unverified capabilities must not prevent saving account changes")
    };
    let saved: Config = serde_json::from_str(&updated).unwrap();
    assert_eq!(saved.review.profile.as_deref(), Some("codex-1"));
    assert_ne!(
        saved.profiles["codex-1"].home,
        dashboard.config.profiles["codex-1"].home
    );
}

#[test]
fn unrelated_account_edits_do_not_allow_saving_a_known_unavailable_review_model() {
    use crate::review_settings::{ReviewSettingsChoices, ReviewSettingsDiscoveryResult};

    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.config.review.profile = Some("codex-1".into());
    dashboard.config.review.enabled = true;
    dashboard.config.review.model = Some("unavailable-model".into());
    let DashboardAction::DiscoverReviewSettings {
        generation,
        profile_id,
        model,
    } = dashboard.begin_review_settings()
    else {
        panic!("expected capability discovery")
    };
    dashboard.apply_review_settings_discovery(
        generation,
        &profile_id,
        model.as_deref(),
        Ok(ReviewSettingsDiscoveryResult::Available {
            choices: ReviewSettingsChoices::default(),
            cleanup_warning: None,
        }),
    );
    if let Mode::Setup(setup) = &mut dashboard.mode {
        setup
            .review_editor
            .as_mut()
            .expect("review editor")
            .form
            .get_mut()
            .focus(crate::review_settings::ReviewSettingsFocus::Back);
    } else {
        panic!("settings remains open after discovery");
    }
    dashboard.handle_key(key(KeyCode::Enter));
    choose(&mut dashboard, "profiles");
    choose(&mut dashboard, "codex-2");
    choose(&mut dashboard, "home");
    dashboard.handle_paste("-changed");
    dashboard.handle_key(key(KeyCode::Enter));
    let action = dashboard.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
    assert_eq!(action, DashboardAction::None);
    let setup = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert!(setup.notice.as_deref().unwrap().contains("unavailable"));
    dashboard.handle_key(key(KeyCode::Backspace));
    choose(&mut dashboard, "codex-1");
    choose(&mut dashboard, "home");
    dashboard.handle_key(key(KeyCode::Enter));
    assert_eq!(
        dashboard.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
        DashboardAction::None,
        "applying an unchanged account field must retain known validation"
    );
}

#[test]
fn expanded_form_preserves_existing_optional_settings() {
    let mut original = config();
    original.phone.tls_cert = Some("/keys/cert.pem".into());
    original.phone.tls_key = Some("/keys/key.pem".into());
    original
        .profiles
        .get_mut("codex-1")
        .unwrap()
        .context_window_bytes = Some(250000);
    let dialog = SetupDialog::new(&original);
    let decoded: Config = config_from_draft(dialog.draft).unwrap();
    assert_eq!(decoded, original);
}

#[test]
fn the_build_cache_page_shows_the_values_its_host_resolves_for_blank_fields() {
    use mj_core::state::{BuildCacheLimit, BuildCachePreview};
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.config.targets.insert(
        "podman-host".into(),
        serde_json::from_value(json!({"kind":"local-podman","image":"example/image:latest"}))
            .unwrap(),
    );
    dashboard.begin_setup();
    choose(&mut dashboard, "targets");
    choose(&mut dashboard, "podman-host");
    let Mode::Setup(dialog) = &mut dashboard.mode else {
        panic!("settings");
    };
    dialog.selected = dialog
        .keys()
        .iter()
        .position(|key| key == "build_cache")
        .unwrap();
    dialog.form.get_mut().focus(SetupControl::List);
    dialog.prepare();
    // Opening the page starts the host lookup exactly once.
    let DashboardAction::PreviewBuildCache {
        generation,
        key: preview_key,
        ..
    } = dashboard.handle_key(key(KeyCode::Enter))
    else {
        panic!("preview build cache");
    };
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Down)),
        DashboardAction::None
    );
    let mut terminal = Terminal::new(TestBackend::new(140, 30)).unwrap();
    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .unwrap();
    let resolving = buffer_lines(terminal.backend().buffer()).join("\n");
    assert!(resolving.contains("Resolving…"), "{resolving}");

    dashboard.build_cache_previewed(
        generation,
        &preview_key,
        Ok(Some(BuildCachePreview {
            native_mbx: Some("1.12.0".into()),
            directory: Some("/mnt/fast/mbx-cache".into()),
            max_size: Some(BuildCacheLimit::HostConfiguration(Some("500GiB".into()))),
            off_reason: Some(
                "the filesystem under /mnt/fast/mbx-cache does not support reflinks".into(),
            ),
        })),
    );
    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .unwrap();
    let resolved = buffer_lines(terminal.backend().buffer()).join("\n");
    for expected in [
        "Enabled                           Off",
        "/mnt/fast/mbx-cache",
        "500GiB, host mbx config",
        "run without the build cache: the filesystem under",
    ] {
        assert!(
            resolved.contains(expected),
            "missing {expected:?} in\n{resolved}"
        );
    }

    // Changing a setting on the page makes the answer stale and asks again.
    let Mode::Setup(dialog) = &mut dashboard.mode else {
        panic!("settings");
    };
    dialog.draft["targets"]["podman-host"]["build_cache"]["max_size"] = json!("1GiB");
    assert!(matches!(
        dashboard.handle_key(key(KeyCode::Down)),
        DashboardAction::PreviewBuildCache { .. }
    ));
}

#[test]
fn empty_archive_after_days_renders_as_never() {
    let draft = serde_json::json!({"sessionwiki": {"archive_after_days": null}});
    assert_eq!(
        value_summary(
            &["sessionwiki".to_owned()],
            "archive_after_days",
            &Value::Null,
            &draft,
            None,
        ),
        "Never"
    );
    assert_eq!(
        value_summary(
            &["phone".to_owned()],
            "tls_cert",
            &Value::Null,
            &serde_json::json!({"phone": {"tls_cert": null}}),
            None,
        ),
        "Automatic / default"
    );
}

#[test]
fn the_sessionwiki_page_estimates_what_an_archive_window_would_reclaim() {
    use mj_core::state::ArchiveSpacePreview;
    let used = ArchiveSpacePreview {
        sessions: 40,
        bytes: 5_153_960_755,
        reclaimable_sessions: 0,
        reclaimable_bytes: 0,
    };
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    dialog.selected = dialog
        .keys()
        .iter()
        .position(|key| key == "sessionwiki")
        .unwrap();
    dialog.form.get_mut().focus(SetupControl::List);
    dialog.prepare();

    // Entering the page measures what sessions use now, exactly once.
    let DashboardAction::PreviewArchiveSpace {
        generation,
        older_than_days: None,
    } = dashboard.handle_key(key(KeyCode::Enter))
    else {
        panic!("entering the page must ask for the space sessions use");
    };
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Down)),
        DashboardAction::None
    );
    let mut terminal = Terminal::new(TestBackend::new(140, 30)).unwrap();
    let mut rendered = |dashboard: &mut DashboardState| {
        terminal
            .draw(|frame| crate::render::render(frame, dashboard))
            .unwrap();
        buffer_lines(terminal.backend().buffer()).join("\n")
    };
    assert!(rendered(&mut dashboard).contains("Resolving…"));

    dashboard.archive_space_previewed(generation, None, Ok(used.clone()));
    let never = rendered(&mut dashboard);
    assert!(
        never.contains("Never · sessions use 4.8G"),
        "the row must report the space sessions use:\n{never}"
    );

    // Typing a number asks again for that number, keystroke by keystroke.
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    dialog.selected = dialog
        .keys()
        .iter()
        .position(|key| key == "archive_after_days")
        .unwrap();
    dialog.form.get_mut().focus(SetupControl::List);
    dialog.prepare();
    dashboard.handle_key(key(KeyCode::Enter));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Char('3'))),
        DashboardAction::PreviewArchiveSpace {
            generation,
            older_than_days: Some(3),
        }
    );
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Char('0'))),
        DashboardAction::PreviewArchiveSpace {
            generation,
            older_than_days: Some(30),
        }
    );

    // The answer for the value already typed past is dropped.
    dashboard.archive_space_previewed(
        generation,
        Some(3),
        Ok(ArchiveSpacePreview {
            reclaimable_sessions: 39,
            reclaimable_bytes: 5_000_000_000,
            ..used.clone()
        }),
    );
    let stale = rendered(&mut dashboard);
    assert!(
        !stale.contains("39 sessions"),
        "an answer for a value typed past must not be shown:\n{stale}"
    );

    dashboard.archive_space_previewed(
        generation,
        Some(30),
        Ok(ArchiveSpacePreview {
            reclaimable_sessions: 12,
            reclaimable_bytes: 1_288_490_188,
            ..used
        }),
    );
    // The open editor covers the page, so while typing the estimate sits
    // under the input, without repeating the number being typed; closing the
    // editor puts the estimate, with the value, back in the row.
    let editing = rendered(&mut dashboard);
    assert!(
        editing.contains("would reclaim 1.2G of 4.8G (12 of 40 sessions)")
            && !editing.contains("30 · would reclaim"),
        "the editor must show what the typed value would reclaim:\n{editing}"
    );
    dashboard.handle_key(key(KeyCode::Enter));
    let reclaim = rendered(&mut dashboard);
    assert!(
        reclaim.contains("30 · would reclaim 1.2G of 4.8G (12 of 40 sessions)"),
        "the row must report what the saved value would reclaim:\n{reclaim}"
    );
}
