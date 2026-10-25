use super::*;

#[test]
fn setup_adds_a_remote_runtime_and_reports_invalid_fields_without_losing_the_draft() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    choose(&mut dashboard, "machines");
    dashboard.handle_key(key(KeyCode::Char('a')));
    dashboard.handle_paste("builder");
    dashboard.handle_key(key(KeyCode::Enter));
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
        panic!("expected background validation, got {action:?}");
    };
    let invalid: Config = serde_json::from_str(&updated).unwrap();
    dashboard.setup_saved(generation, Err(invalid.validate().unwrap_err().to_string()));
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings");
    };
    assert!(dialog.notice.as_ref().unwrap().contains("SSH host"));
    choose(&mut dashboard, "host");
    dashboard.handle_paste("builder.example.test");
    dashboard.handle_key(key(KeyCode::Enter));
    activate(&mut dashboard, SetupControl::Back);
    activate(&mut dashboard, SetupControl::Back);

    // A runtime on the new machine: Docker, chosen from the runtime kinds,
    // and the machine, chosen from the machines the draft now has.
    choose(&mut dashboard, "targets");
    dashboard.handle_key(key(KeyCode::Char('a')));
    dashboard.handle_paste("builder-docker");
    dashboard.handle_key(key(KeyCode::Enter));
    choose(&mut dashboard, "kind");
    select_choice(&mut dashboard, "docker");
    choose(&mut dashboard, "machine");
    select_choice(&mut dashboard, "builder");

    let action = dashboard.handle_key(crossterm::event::KeyEvent::new(
        KeyCode::Char('s'),
        KeyModifiers::CONTROL,
    ));
    let DashboardAction::SaveSetup { updated, .. } = action else {
        panic!("{action:?}");
    };
    let saved: Config = serde_json::from_str(&updated).unwrap();
    assert!(
        matches!(&saved.targets["builder-docker"], mj_core::config::TargetTemplate::SshDocker { ssh, .. } if ssh.host == "builder.example.test")
    );
    let generation = setup_dialog_mut(&mut dashboard.mode).unwrap().generation;
    dashboard.setup_saved(generation, Err("disk full".into()));
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings");
    };
    assert!(!dialog.saving);
    assert_eq!(
        dialog.draft["machines"]["builder"]["host"],
        "builder.example.test"
    );
    assert!(dialog.notice.as_ref().unwrap().contains("disk full"));
}

/// Campaign finding W-1: the second Esc out of a dirty Settings draft opens
/// the "Discard Settings changes?" prompt, which looks like the dashboard
/// behind it. Esc on that prompt means "Keep editing", so a third Esc returns
/// to Settings. Once the draft is discarded, Esc on the dashboard is inert.
#[test]
fn escape_on_the_discard_prompt_keeps_editing_and_dashboard_escape_stays_inert() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    choose(&mut dashboard, "review");
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Down));
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Esc));
    dashboard.handle_key(key(KeyCode::Esc));
    assert!(matches!(dashboard.mode, Mode::Confirm(_)));
    dashboard.handle_key(key(KeyCode::Esc));
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("Esc on the discard prompt keeps editing")
    };
    assert_eq!(dialog.draft["review"]["trigger"], "turn");
    // Discard, then Esc on the plain dashboard does nothing.
    dashboard.handle_key(key(KeyCode::Esc));
    assert!(matches!(dashboard.mode, Mode::Confirm(_)));
    dashboard.handle_key(key(KeyCode::Right));
    dashboard.handle_key(key(KeyCode::Enter));
    assert!(!dashboard.modal_open());
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Esc)),
        DashboardAction::None
    );
    assert!(!dashboard.modal_open());
}

/// A refused value is reported with the field it was typed into, not on the
/// dialog's bottom rows a page below it.
// Hard-won: 9e96756a: field validation errors appeared far below the edited field.
#[test]
fn a_rejected_field_value_is_reported_under_the_field() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    choose(&mut dashboard, "machines");
    choose(&mut dashboard, "local");
    choose(&mut dashboard, "build_cache");
    choose(&mut dashboard, "max_total_size");
    let background_notice = setup_dialog_mut(&mut dashboard.mode)
        .unwrap()
        .notice
        .clone();
    for typed in ['1', '.', '5'] {
        dashboard.handle_key(key(KeyCode::Char(typed)));
    }
    dashboard.handle_key(key(KeyCode::Enter));
    let dialog = setup_dialog_mut(&mut dashboard.mode).expect("settings");
    assert_eq!(
        focused_field(dialog).error.as_deref(),
        Some("Enter a whole number of gigabytes.")
    );
    assert_eq!(
        dialog.notice, background_notice,
        "field validation must not replace a background notice"
    );
    let lines = drawn(&mut dashboard, 140, 30);
    let (_, field) = point(&lines, "1.5");
    let (_, message) = point(&lines, "Enter a whole number of gigabytes.");
    assert_eq!(
        message,
        field + 1,
        "the message is {} rows from the field:\n{}",
        i32::from(message) - i32::from(field),
        lines.join("\n")
    );
}

/// The numeric settings are edited as text but saved as numbers, "Use
/// default" puts the default back, and text that is not a number is refused
/// under the field without losing the draft. Launch campaign finding C-23.
// Hard-won: ef88371a: numeric field text failed to convert during save.
#[test]
fn numeric_settings_round_trip_use_their_default_and_refuse_text() {
    type Read = fn(&Config) -> u64;
    let fields: [(&str, &str, Read, u64); 3] = [
        ("notify", "delay_seconds", |c| c.notify.delay_seconds, 2),
        (
            "sessionwiki",
            "archive_after_days",
            |c| c.sessionwiki.archive_after_days.map_or(0, u64::from),
            0,
        ),
        (
            "subagents",
            "max_concurrent",
            |c| u64::try_from(c.subagents.max_concurrent).unwrap(),
            6,
        ),
    ];
    for (section, key, read, default) in fields {
        let mut dialog = SetupDialog::new(&config());
        edit_field(&mut dialog, section, key, "5");
        apply_focused(&mut dialog, false).unwrap();
        assert_eq!(dialog.draft[section][key], 5, "{section}.{key}");
        assert_eq!(read(&saved_config(&mut dialog)), 5, "{section}.{key}");

        edit_field(&mut dialog, section, key, "5");
        apply_focused(&mut dialog, true).unwrap();
        assert_eq!(read(&saved_config(&mut dialog)), default, "{section}.{key}");

        edit_field(&mut dialog, section, key, "five");
        let error = apply_focused(&mut dialog, false).unwrap_err();
        assert!(error.starts_with("Enter a whole number"), "{error}");
        assert!(
            focused_field(&dialog).error.is_some(),
            "{section}.{key}: the invalid field retains its error"
        );
    }
}

#[test]
fn user_managed_budgets_show_host_values_and_cannot_be_edited() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    choose(&mut dashboard, "machines");
    choose(&mut dashboard, "local");
    choose(&mut dashboard, "build_cache");
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    let (_, preview_key) = dialog.build_cache_page().unwrap();
    dialog.build_cache_previews.insert(
        "local".to_owned(),
        BuildCachePreviewState {
            key: preview_key,
            install_mbx_available: false,
            result: BuildCachePreviewResult::Ready(Some(Box::new(
                mj_core::state::BuildCachePreview {
                    native_mbx: Some("1.21.0".into()),
                    mbx_profile_file: None,
                    mbx_profile_warning: None,
                    mbx_manual_path_line: None,
                    directory: Some("/native/cache".into()),
                    max_total_size: Some(mj_core::state::BuildCacheLimit::HostConfiguration(Some(
                        "500GB".into(),
                    ))),
                    user_managed: true,
                    application: mj_core::state::BuildCacheApplication::Applied,
                    budget_note: None,
                    stats: None,
                    off_reason: None,
                },
            ))),
        },
    );
    dialog.prepare();
    let text = drawn(&mut dashboard, 140, 30).join("\n");
    assert!(text.contains("User-managed mbx"), "{text}");
    assert!(text.contains("500 GB, host mbx config"), "{text}");
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert!(
        dialog
            .descriptors
            .iter()
            .any(|c| c.path.last().is_some_and(|key| key == "max_total_size")
                && c.kind == SettingKind::ReadOnly)
    );
    for _ in 0..8 {
        dashboard.handle_key(key(KeyCode::Tab));
    }
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert!(
        dialog
            .focused_path()
            .is_none_or(|path| path.last().is_none_or(|key| key != "max_total_size"))
    );
    assert_eq!(
        dialog.draft["machines"]["local"]["build_cache"]["max_total_size"],
        Value::Null
    );
}

#[test]
fn pending_text_survives_back_counts_as_dirty_and_does_not_trigger_shortcuts() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_settings_section("phone", None);
    choose(&mut dashboard, "bind");
    replace_text(&mut dashboard, "a/path");
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert!(dialog.is_dirty());
    assert!(dialog.search.is_none());
    assert!(dialog.editor.is_none());
    assert_ne!(dialog.draft["phone"]["bind"], "a/path");
    dashboard.handle_key(key(KeyCode::Esc));
    choose(&mut dashboard, "phone");
    choose(&mut dashboard, "bind");
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert_eq!(focused_field(dialog).input.value(), "a/path");
    dashboard.handle_key(key(KeyCode::Esc));
    dashboard.handle_key(key(KeyCode::Esc));
    assert!(
        matches!(dashboard.mode, Mode::Confirm(_)),
        "pending text needs the Settings discard guard"
    );
}

#[test]
fn save_refuses_invalid_pending_text_and_reveals_the_field_on_its_page() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_settings_section("subagents", None);
    choose(&mut dashboard, "max_concurrent");
    replace_text(&mut dashboard, "many");
    dashboard.handle_key(key(KeyCode::Esc));
    let action = dashboard.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
    assert_eq!(action, DashboardAction::None);
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert!(!dialog.saving);
    assert_eq!(dialog.path, ["subagents"]);
    assert_eq!(
        dialog.focused_path().unwrap(),
        ["subagents", "max_concurrent"]
    );
    assert_eq!(focused_field(dialog).input.value(), "many");
    assert!(
        focused_field(dialog)
            .error
            .as_deref()
            .unwrap()
            .contains("whole number")
    );
    let rendered = drawn(&mut dashboard, 100, 20).join("\n");
    assert!(rendered.contains("many"), "{rendered}");
    assert!(rendered.contains("Enter a whole number"), "{rendered}");
    dashboard.handle_key(key(KeyCode::Esc));
    choose(&mut dashboard, "review");
    assert!(
        setup_dialog_mut(&mut dashboard.mode)
            .unwrap()
            .review_editor
            .is_some()
    );
    assert_eq!(
        dashboard.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
        DashboardAction::None
    );
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert!(
        dialog.review_editor.is_none(),
        "Save must reveal the invalid Settings field even from Code Review"
    );
    assert_eq!(
        dialog.focused_path().unwrap(),
        ["subagents", "max_concurrent"]
    );
}

#[test]
fn enter_reveals_validation_when_the_focused_field_is_scrolled_away() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_settings_section("profiles", Some("codex-1"));
    choose(&mut dashboard, "context_window_bytes");
    replace_text(&mut dashboard, "many");
    let lines = drawn(&mut dashboard, 100, 14);
    let (column, row) = point(&lines, "many");
    for _ in 0..8 {
        dashboard.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        });
        drawn(&mut dashboard, 100, 14);
    }
    let scrolled = drawn(&mut dashboard, 100, 14);
    assert_ne!(scrolled, lines, "scrolling must actually move the content");
    assert!(
        !scrolled.iter().any(|line| line.contains("many")),
        "the pending field must scroll out of view: {scrolled:#?}"
    );
    dashboard.handle_key(key(KeyCode::Enter));
    let rendered = drawn(&mut dashboard, 100, 14).join("\n");
    assert!(rendered.contains("many"), "{rendered}");
    assert!(rendered.contains("Enter a whole number"), "{rendered}");
}

// Hard-won: Settings review: pre-event preparation erased the field cursor map.
#[test]
fn clicking_inside_an_inline_field_places_the_cursor_at_that_character() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_settings_section("phone", None);
    choose(&mut dashboard, "bind");
    replace_text(&mut dashboard, "abcdef");
    let lines = drawn(&mut dashboard, 140, 40);
    let (column, row) = point(&lines, "abcdef");
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        dashboard.handle_mouse(MouseEvent {
            kind,
            column: column + 3,
            row,
            modifiers: KeyModifiers::NONE,
        });
        drawn(&mut dashboard, 140, 40);
    }
    dashboard.handle_key(key(KeyCode::Char('X')));
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert_eq!(focused_field(dialog).input.value(), "abcXdef");
}

// Hard-won: Settings review: removing Clear made nullable Permissions impossible to reset.
#[test]
fn nullable_permissions_dropdown_restores_default_after_a_concrete_choice() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.config.targets.insert(
        "bare".into(),
        serde_json::from_value(
            json!({"kind":"ssh-bare","host":"builder","permissions":"guardian"}),
        )
        .unwrap(),
    );
    dashboard.begin_settings_section("targets", Some("bare"));
    choose(&mut dashboard, "permissions");
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert!(open_choices(dialog).0.contains(&Value::Null));
    let label = dialog.choice_label(
        &["targets".into(), "bare".into(), "permissions".into()],
        &Value::Null,
    );
    assert_eq!(label, "Default (Ask for approvals)");
    assert!(drawn(&mut dashboard, 140, 40).join("\n").contains(&label));
    select_value(&mut dashboard, &Value::Null);
    let saved = saved_config(setup_dialog_mut(&mut dashboard.mode).unwrap());
    assert!(
        matches!(
            &saved.targets["bare"],
            mj_core::config::TargetTemplate::SshBare {
                permissions: mj_core::config::PermissionMode::Guardian,
                ..
            }
        ),
        "the default must resolve through the normal configuration reader"
    );
    choose(&mut dashboard, "permissions");
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    let (values, selected) = open_choices(dialog);
    assert_eq!(
        values[selected],
        Value::Null,
        "an unset value must display its Default choice"
    );
    select_choice(&mut dashboard, "yolo");
    choose(&mut dashboard, "permissions");
    select_value(&mut dashboard, &Value::Null);
    assert!(
        setup_dialog_mut(&mut dashboard.mode).unwrap().draft["targets"]["bare"]["permissions"]
            .is_null()
    );
}

// Hard-won: Settings live test: corrected archive text left a duplicate validation notice.
#[test]
fn inline_validation_clears_with_its_field_without_replacing_background_notices() {
    for (save, background_failure) in [(false, false), (true, false), (false, true), (true, true)] {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.begin_settings_section("sessionwiki", None);
        choose(&mut dashboard, "archive_after_days");
        replace_text(&mut dashboard, "invalid-days");
        let generation = setup_dialog_mut(&mut dashboard.mode).unwrap().generation;
        if background_failure {
            dashboard.archive_space_previewed(generation, None, Err("archive read failed".into()));
        }
        let notice = setup_dialog_mut(&mut dashboard.mode)
            .unwrap()
            .notice
            .clone();
        if background_failure {
            assert!(notice.as_deref().unwrap().contains("archive read failed"));
        } else {
            assert!(notice.is_none());
        }
        dashboard.handle_key(if save {
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)
        } else {
            key(KeyCode::Enter)
        });
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        assert!(
            focused_field(dialog)
                .error
                .as_deref()
                .unwrap()
                .contains("Enter a whole number of days")
        );
        assert_eq!(
            dialog.notice, notice,
            "inline validation must not create or replace a page notice"
        );
        let rendered = drawn(&mut dashboard, 140, 42).join("\n");
        assert_eq!(
            rendered.matches("Enter a whole number of days").count(),
            1,
            "the validation message should appear only beneath the field: {rendered}"
        );
        replace_text(&mut dashboard, "8");
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        assert!(focused_field(dialog).error.is_none());
        assert_eq!(dialog.notice, notice);
        dashboard.archive_space_previewed(
            generation,
            Some(8),
            Ok(mj_core::state::ArchiveSpacePreview {
                sessions: 0,
                bytes: 0,
                reclaimable_sessions: 0,
                reclaimable_bytes: 0,
            }),
        );
        let rendered = drawn(&mut dashboard, 140, 42).join("\n");
        assert!(
            !rendered.contains("Enter a whole number of days"),
            "corrected pending text must remove its validation error: {rendered}"
        );
        assert!(
            rendered.contains("would reclaim"),
            "the estimate must follow the pending 8: {rendered}"
        );
        dashboard.handle_key(key(KeyCode::Enter));
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        assert!(focused_field(dialog).error.is_none());
        assert_eq!(dialog.draft["sessionwiki"]["archive_after_days"], 8);
        assert_eq!(
            dialog.notice, notice,
            "successful field validation must preserve unrelated background notices too"
        );
    }
}
