use super::*;

/// Workers read the Jev switch when they start, so a running session keeps
/// classifying until it is resumed or restarted. The page and the save
/// notice say so instead of "Off: nothing is sent". Launch re-verification
/// finding R3-8.
// Hard-won: 8dccf5db: the save notice omitted when running sessions would follow the jev change.
#[test]
fn turning_jev_off_says_running_sessions_follow_after_a_resume_or_restart() {
    let jev = schema::help(&["jev".to_owned()]);
    assert!(!jev.contains("nothing is sent"), "{jev}");
    assert!(jev.contains("resume or restart"), "{jev}");

    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_settings_section("jev", None);
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    dialog.draft["jev"]["enabled"] = json!(false);
    let generation = dialog.generation;
    let saved = saved_config(dialog);
    assert!(!saved.jev.enabled);
    dashboard.setup_saved(generation, Ok(saved));
    let notice = dashboard.notice().unwrap();
    assert!(
        notice.contains("running sessions follow it after their next resume or restart"),
        "{notice}"
    );

    // A save that leaves Jev alone does not mention it.
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    let generation = dialog.generation;
    let saved = saved_config(dialog);
    dashboard.setup_saved(generation, Ok(saved));
    assert!(!dashboard.notice().unwrap().contains("Jev"));
}

/// Launch campaign finding A-3: once `ascii` was chosen, the popup offered no
/// way back to "Follows the terminal", and saving that choice must remove the
/// key rather than write a third value.
// Hard-won: 688cfd2a: the ASCII choice could not be cleared back to terminal-following behavior.
#[test]
fn symbols_can_return_to_the_unset_state_and_saving_removes_the_key() {
    let mut dashboard = dashboard_with_session(stopped_session());
    let mut config = dashboard.config.clone();
    config.advanced.symbols = Some(mj_core::config::SymbolSet::Ascii);
    dashboard.set_config(config);
    dashboard.begin_setup();
    choose(&mut dashboard, "advanced");
    choose(&mut dashboard, "symbols");
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .unwrap();
    let popup = buffer_lines(terminal.backend().buffer()).join("\n");
    assert!(popup.contains("Follows the terminal"), "{popup}");
    assert!(
        popup.contains("unicode") && popup.contains("ascii"),
        "{popup}"
    );

    dashboard.handle_key(key(KeyCode::Up));
    dashboard.handle_key(key(KeyCode::Up));
    dashboard.handle_key(key(KeyCode::Enter));
    assert_eq!(
        setup_dialog_mut(&mut dashboard.mode).unwrap().draft["advanced"]["symbols"],
        Value::Null
    );
    let action = dashboard.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
    let DashboardAction::SaveSetup { updated, .. } = action else {
        panic!("expected Settings save, got {action:?}")
    };
    let saved: Config = serde_json::from_str(&updated).unwrap();
    assert_eq!(saved.advanced.symbols, None);
    assert!(!updated.contains("symbols"), "{updated}");
}

/// The first page describes the draft, not the saved file, and a limit put
/// back to its default still shows the limit. Launch campaign finding C-22.
// Hard-won: e5bdf191: the first-page summary read saved values instead of the active draft.
#[test]
fn the_first_page_summarizes_drafted_values_before_they_are_saved() {
    let mut dialog = SetupDialog::new(&config());
    edit_field(&mut dialog, "sessionwiki", "archive_after_days", "30");
    apply_focused(&mut dialog, false).unwrap();
    edit_field(&mut dialog, "subagents", "max_concurrent", "4");
    apply_focused(&mut dialog, false).unwrap();
    let summary = |dialog: &SetupDialog, key: &str| {
        row_summary(&[], key, &dialog.draft[key], &dialog.draft, None)
    };
    assert_eq!(summary(&dialog, "sessionwiki"), "Archives after 30 days");
    assert_eq!(summary(&dialog, "subagents"), "Up to 4 at once");

    edit_field(&mut dialog, "subagents", "max_concurrent", "");
    apply_focused(&mut dialog, true).unwrap();
    assert_eq!(summary(&dialog, "subagents"), "Up to 6 at once");
}

/// A long save error is shown whole: the notice grows to fit it instead of
/// stopping after three lines. Launch campaign finding C-20.
// Hard-won: 006bb14c: save notices were truncated after three rows.
#[test]
fn a_long_save_error_is_shown_whole() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    let error = format!(
        "Could not save: {} closing words",
        "Setup would change the project that a running session uses. ".repeat(5)
    );
    setup_dialog_mut(&mut dashboard.mode).unwrap().notice = Some(error);
    let lines = drawn(&mut dashboard, 120, 40);
    assert!(
        lines.iter().any(|line| line.contains("closing words")),
        "{}",
        lines.join("\n")
    );
    // The footer's controls are still drawn below it.
    assert!(
        lines.iter().any(|line| line.contains("Save")),
        "{}",
        lines.join("\n")
    );
}

// Hard-won: #1153: saving unrelated Settings fields overwrote named-instance web listener ports.
#[test]
fn named_instance_settings_preserve_the_default_and_explicit_web_listener() {
    const CHILD: &str = "MJ_TEST_SETTINGS_INSTANCE_CHILD";
    const INSTANCE: &str = "settings-1153";
    if std::env::var_os(CHILD).is_none() {
        // Instance selection is process-wide: exercise a named instance in
        // its own process rather than changing the other tests' environment.
        let output = mj_core::subprocess::run_with_input(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "setup::tests::named_instance_settings_preserve_the_default_and_explicit_web_listener",
                    "--exact",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("MJ_INSTANCE", INSTANCE),
            &[],
        )
        .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let default = mj_core::config::default_phone_bind_for(Some(INSTANCE));
    for bind in [&default, &"127.0.0.1:39777".to_owned()] {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.config.phone = mj_core::config::PhoneConfig::default();
        dashboard.config.phone.bind.clone_from(bind);
        if bind == &default {
            assert!(
                serde_json::to_value(&*dashboard.config)
                    .unwrap()
                    .get("phone")
                    .is_none()
            );
        }
        dashboard.begin_setup();
        let rendered = drawn(&mut dashboard, 140, 48).join("\n");
        assert!(rendered.contains(&format!("On · {bind}")), "{rendered}");
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        assert!(
            !dialog.is_dirty(),
            "opening Settings must preserve defaults"
        );
        dialog.draft["keys"]["prefix"] = json!("ctrl+]");
        let saved = saved_config(dialog);
        assert_eq!(&saved.phone.bind, bind);
        assert_eq!(saved.keys.prefix, "ctrl+]");
    }
}

#[test]
fn subagents_settings_reuses_cached_efforts_across_reopens_and_model_switches() {
    use mj_core::profile_capabilities::{
        CapabilityState, ProfileCapabilities, ProfileCapabilitiesSnapshot,
    };
    use mj_core::worker_launch::ProfileConfig;

    let mut dashboard = dashboard_with_session(stopped_session());
    let choice = |value: &str| mj_core::acp::SessionConfigChoice {
        value: value.into(),
        name: value.into(),
        description: None,
    };
    let profiles = dashboard
        .config
        .enabled_profiles()
        .map(|(id, profile)| {
            (
                profile.capabilities_key(id),
                ProfileCapabilities {
                    choices: CapabilityState::Ready(ProfileConfig {
                        model: Some("fast".into()),
                        models: vec![choice("fast"), choice("smart")],
                        efforts: vec![],
                        observed_at: 1,
                    }),
                    efforts: std::collections::BTreeMap::from([
                        ("fast".into(), CapabilityState::Ready(vec![choice("low")])),
                        (
                            "smart".into(),
                            CapabilityState::Ready(vec![choice("high"), choice("max")]),
                        ),
                    ]),
                },
            )
        })
        .collect();
    dashboard.set_profile_capabilities(ProfileCapabilitiesSnapshot { profiles });

    // Opening the subagents dialog populates options from the process cache immediately.
    dashboard.config.subagents.straightforward = vec![mj_core::config::SubagentChoice {
        model: "fast".into(),
        effort: None,
    }];
    dashboard.begin_settings_section("subagents", None);
    assert_eq!(dashboard.take_profile_hydration(), None);
    choose(&mut dashboard, "straightforward");
    let model_path = vec![
        "subagents".into(),
        "straightforward".into(),
        "0".into(),
        "model".into(),
    ];
    setup_dialog_mut(&mut dashboard.mode)
        .unwrap()
        .focus_path(&model_path);
    dashboard.handle_key(key(KeyCode::Enter));
    select_choice(&mut dashboard, "smart");
    assert_eq!(dashboard.take_profile_hydration(), None);

    let effort_path = vec![
        "subagents".into(),
        "straightforward".into(),
        "0".into(),
        "effort".into(),
    ];
    setup_dialog_mut(&mut dashboard.mode)
        .unwrap()
        .focus_path(&effort_path);
    dashboard.handle_key(key(KeyCode::Enter));
    let (efforts, _) = open_choices(setup_dialog_mut(&mut dashboard.mode).unwrap());
    assert_eq!(efforts, vec![Value::Null, json!("high"), json!("max")]);

    // Closing and reopening preserves the cached model and effort options.
    dashboard.handle_key(key(KeyCode::Esc));
    dashboard.handle_key(key(KeyCode::Esc));
    dashboard.handle_key(key(KeyCode::Esc));
    dashboard.begin_settings_section("subagents", None);
    assert_eq!(dashboard.take_profile_hydration(), None);
    choose(&mut dashboard, "straightforward");
    setup_dialog_mut(&mut dashboard.mode)
        .unwrap()
        .focus_path(&model_path);
    dashboard.handle_key(key(KeyCode::Enter));
    let (models, _) = open_choices(setup_dialog_mut(&mut dashboard.mode).unwrap());
    assert_eq!(models, vec![json!("fast"), json!("smart")]);
}

#[test]
fn subagent_single_page_rejects_partly_configured_rows_and_handles_remove_and_narrow_layout() {
    use mj_core::profile_capabilities::{
        CapabilityState, ProfileCapabilities, ProfileCapabilitiesSnapshot,
    };
    use mj_core::worker_launch::ProfileConfig;

    let mut dashboard = dashboard_with_session(stopped_session());
    let choice = |value: &str| mj_core::acp::SessionConfigChoice {
        value: value.into(),
        name: value.into(),
        description: None,
    };
    let profiles = dashboard
        .config
        .enabled_profiles()
        .map(|(id, profile)| {
            (
                profile.capabilities_key(id),
                ProfileCapabilities {
                    choices: CapabilityState::Ready(ProfileConfig {
                        model: Some("fast".into()),
                        models: vec![choice("fast"), choice("smart")],
                        efforts: vec![],
                        observed_at: 1,
                    }),
                    efforts: std::collections::BTreeMap::from([
                        ("fast".into(), CapabilityState::Ready(vec![choice("low")])),
                        ("smart".into(), CapabilityState::Ready(vec![choice("high")])),
                    ]),
                },
            )
        })
        .collect();
    dashboard.set_profile_capabilities(ProfileCapabilitiesSnapshot { profiles });
    dashboard.begin_settings_section("subagents", None);
    choose(&mut dashboard, "straightforward");
    assert_eq!(
        setup_dialog_mut(&mut dashboard.mode).unwrap().path,
        ["subagents", "straightforward"]
    );

    // Add stays on the category page, marks the draft dirty, and focuses row 0 model.
    dashboard.handle_key(key(KeyCode::Char('a')));
    {
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        assert_eq!(dialog.path, ["subagents", "straightforward"]);
        assert!(dialog.is_dirty(), "incomplete row must count as dirty");
        assert_eq!(
            dialog.focused_path(),
            Some(vec![
                "subagents".into(),
                "straightforward".into(),
                "0".into(),
                "model".into(),
            ])
        );
    }

    // Esc, Backspace, Back button, and Ctrl+S all reject the partly configured row.
    for trigger in [
        key(KeyCode::Esc),
        key(KeyCode::Backspace),
        KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
    ] {
        let action = dashboard.handle_key(trigger);
        assert_eq!(action, DashboardAction::None);
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        assert_eq!(dialog.path, ["subagents", "straightforward"]);
        assert!(
            dialog
                .notice
                .as_deref()
                .is_some_and(|n| n.contains("Straightforward entry 1")),
            "expected incomplete-row notice, got {:?}",
            dialog.notice
        );
    }

    // A background capability refresh with a discovery error must not clobber the active notice.
    {
        let mut unreachable_profiles = dashboard.profile_capabilities.profiles.clone();
        for caps in unreachable_profiles.values_mut() {
            caps.choices = CapabilityState::Failed("worker unavailable".into());
        }
        let saved = dashboard.profile_capabilities.clone();
        dashboard.set_profile_capabilities(ProfileCapabilitiesSnapshot {
            profiles: unreachable_profiles.clone(),
        });
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        assert!(
            dialog
                .notice
                .as_deref()
                .is_some_and(|n| n.contains("Straightforward entry 1")),
            "background discovery error overwrote active validation notice: {:?}",
            dialog.notice
        );
        // Existing discovery notices still refresh to new failure details and clear on recovery.
        dialog.notice = Some("Discovery: pending".into());
        dashboard.set_profile_capabilities(ProfileCapabilitiesSnapshot {
            profiles: unreachable_profiles,
        });
        assert!(
            setup_dialog_mut(&mut dashboard.mode)
                .unwrap()
                .notice
                .as_deref()
                .is_some_and(|n| n.contains("worker unavailable"))
        );
        dashboard.set_profile_capabilities(saved);
        assert_eq!(setup_dialog_mut(&mut dashboard.mode).unwrap().notice, None);
    }

    // Jumping away via search and trying to save still rejects and jumps back to the incomplete row.
    dashboard.handle_key(key(KeyCode::Char('/')));
    for ch in "theme".chars() {
        dashboard.handle_key(key(KeyCode::Char(ch)));
    }
    dashboard.handle_key(key(KeyCode::Enter));
    assert_eq!(
        setup_dialog_mut(&mut dashboard.mode).unwrap().path,
        ["interface"]
    );
    let action = dashboard.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
    assert_eq!(action, DashboardAction::None);
    {
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        assert_eq!(dialog.path, ["subagents", "straightforward"]);
        assert_eq!(
            dialog.focused_path(),
            Some(vec![
                "subagents".into(),
                "straightforward".into(),
                "0".into(),
                "model".into(),
            ])
        );
    }

    // Removing the only incomplete row via Delete unblocks Back and clears dirty state.
    dashboard.handle_key(key(KeyCode::Delete));
    {
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        assert_eq!(dialog.path, ["subagents", "straightforward"]);
        assert_eq!(dialog.draft["subagents"]["straightforward"], json!([]));
        assert!(!dialog.is_dirty());
    }
    dashboard.handle_key(key(KeyCode::Esc));
    assert_eq!(
        setup_dialog_mut(&mut dashboard.mode).unwrap().path,
        ["subagents"]
    );

    // Re-enter, configure two rows, verify narrow rendering, and remove row 0 to check index shifting.
    choose(&mut dashboard, "straightforward");
    dashboard.handle_key(key(KeyCode::Char('a')));
    dashboard.handle_key(key(KeyCode::Enter));
    select_choice(&mut dashboard, "fast");
    dashboard.handle_key(key(KeyCode::Char('a')));
    dashboard.handle_key(key(KeyCode::Enter));
    select_choice(&mut dashboard, "smart");
    let effort_1 = vec![
        "subagents".into(),
        "straightforward".into(),
        "1".into(),
        "effort".into(),
    ];
    setup_dialog_mut(&mut dashboard.mode)
        .unwrap()
        .focus_path(&effort_1);
    dashboard.handle_key(key(KeyCode::Enter));
    select_choice(&mut dashboard, "high");

    // Narrow layout renders stacked model/effort/remove controls without overlapping or panicking.
    let narrow = drawn(&mut dashboard, 60, 30).join("\n");
    assert!(narrow.contains("1 model"), "{narrow}");
    assert!(narrow.contains("1 effort"), "{narrow}");
    assert!(narrow.contains("Remove 1"), "{narrow}");
    assert!(narrow.contains("2 model"), "{narrow}");

    // Removing row 0 shifts row 1 ("smart" / "high") into index 0 and keeps the page open.
    let model_0 = vec![
        "subagents".into(),
        "straightforward".into(),
        "0".into(),
        "model".into(),
    ];
    setup_dialog_mut(&mut dashboard.mode)
        .unwrap()
        .focus_path(&model_0);
    dashboard.handle_key(key(KeyCode::Delete));
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert_eq!(dialog.path, ["subagents", "straightforward"]);
    assert_eq!(
        dialog.draft["subagents"]["straightforward"],
        json!([{"model": "smart", "effort": "high"}])
    );
}
