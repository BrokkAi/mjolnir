use super::*;
use crate::Mode;
use crate::test_support::{buffer_lines, config, drawn, key};
use crossterm::event::KeyCode;
use mj_core::profile_capabilities::{
    CapabilityState, ProfileCapabilities, ProfileCapabilitiesSnapshot,
};
use mj_core::worker_launch::ProfileConfig;
use ratatui::{Terminal, backend::TestBackend};

fn choice(value: &str) -> SessionConfigChoice {
    SessionConfigChoice {
        value: value.into(),
        name: value.into(),
        description: None,
    }
}

fn catalog(config: &Config, models: &[&str]) -> ProfileCapabilitiesSnapshot {
    ProfileCapabilitiesSnapshot {
        profiles: config
            .enabled_profiles()
            .map(|(id, profile)| {
                (
                    profile.capabilities_key(id),
                    ProfileCapabilities {
                        choices: CapabilityState::Ready(ProfileConfig {
                            model: models.first().map(|model| (*model).into()),
                            models: models.iter().map(|model| choice(model)).collect(),
                            efforts: vec![],
                            observed_at: 1,
                        }),
                        efforts: models
                            .iter()
                            .map(|model| {
                                (
                                    (*model).into(),
                                    CapabilityState::Ready(vec![
                                        choice("medium"),
                                        choice("high"),
                                        choice("xhigh"),
                                    ]),
                                )
                            })
                            .collect(),
                    },
                )
            })
            .collect(),
    }
}

#[test]
fn autoconfigure_button_fills_rows_with_offered_efforts() {
    let config = config();
    let mut dialog = ReviewSettingsDialog::new(&config);
    assert!(
        !dialog
            .form
            .borrow()
            .is_enabled(ReviewSettingsFocus::Autoconfigure)
    );
    dialog.update_choices(&config, &catalog(&config, &["gpt-6-terra", "gpt-6-sol"]));
    assert!(dialog.review.reviewers.is_empty());
    let suggested = dialog.suggested().unwrap();
    assert_eq!(suggested[0].model, "gpt-6-terra");
    assert_eq!(suggested[0].effort.as_deref(), Some("xhigh"));
    assert!(
        dialog
            .form
            .borrow()
            .is_enabled(ReviewSettingsFocus::Autoconfigure)
    );
    dialog.update_choices(&config, &catalog(&config, &["gpt-6-terra", "gpt-6-sol"]));
    assert!(dialog.review.reviewers.is_empty());
}

#[test]
fn opening_with_reviewers_preserves_them_when_catalog_arrives() {
    let mut config = config();
    config.review.reviewers = vec![SubagentChoice {
        model: "custom-model".into(),
        effort: Some("high".into()),
    }];
    let mut dialog = ReviewSettingsDialog::new(&config);
    dialog.update_choices(&config, &catalog(&config, &["gpt-6-terra", "gpt-6-sol"]));
    assert_eq!(dialog.review.reviewers, config.review.reviewers);
}

#[test]
fn autoconfigure_disabled_without_supported_models() {
    let config = config();
    let mut dialog = ReviewSettingsDialog::new(&config);
    dialog.update_choices(&config, &catalog(&config, &["other-model"]));
    assert!(dialog.review.reviewers.is_empty());
    assert!(
        !dialog
            .form
            .borrow()
            .is_enabled(ReviewSettingsFocus::Autoconfigure)
    );
    assert!(dialog.can_save()); // Off accepts an empty list.
    dialog.review.trigger = ReviewTrigger::Turn;
    assert!(!dialog.can_save());
}

#[test]
fn effort_choices_follow_the_selected_model() {
    let config = config();
    let mut catalog = catalog(&config, &["gpt-6-terra", "gpt-6-sol"]);
    for profile in catalog.profiles.values_mut() {
        profile.efforts.insert(
            "gpt-6-terra".into(),
            CapabilityState::Ready(vec![choice("xhigh")]),
        );
        profile.efforts.insert(
            "gpt-6-sol".into(),
            CapabilityState::Ready(vec![choice("medium")]),
        );
    }
    let mut dialog = ReviewSettingsDialog::new(&config);
    dialog.update_choices(&config, &catalog);
    dialog.review.reviewers = dialog.suggested().unwrap();
    dialog.update_choices(&config, &catalog);
    assert_eq!(
        dialog.choices(ReviewSettingsFocus::Effort(0)).0,
        ["Default", "xhigh"]
    );
    assert_eq!(
        dialog.choices(ReviewSettingsFocus::Effort(1)).0,
        ["Default", "medium"]
    );
}

#[test]
fn reviewer_rows_can_be_added_removed_and_reconfigured() {
    let mut dashboard = DashboardState::new(config(), Default::default(), Default::default());
    dashboard.begin_setup();
    dashboard.begin_setup_review();
    let snapshot = catalog(&dashboard.config, &["gpt-6-terra", "gpt-6-sol"]);
    dashboard.set_profile_capabilities(snapshot.clone());
    let config = dashboard.config.clone();
    let Mode::Setup(setup) = &mut dashboard.mode else {
        panic!("setup")
    };
    let editor = setup.review_editor.as_mut().unwrap();
    editor.review.reviewers = editor.suggested().unwrap();
    editor.update_choices(&config, &snapshot);
    assert_eq!(editor.review.reviewers.len(), 2);
    editor.form.get_mut().focus(ReviewSettingsFocus::Add);
    dashboard.handle_key(key(KeyCode::Enter));
    drawn(&mut dashboard, 100, 30);
    let Mode::Setup(setup) = &mut dashboard.mode else {
        panic!("setup")
    };
    let editor = setup.review_editor.as_mut().unwrap();
    assert_eq!(editor.review.reviewers.len(), 3);
    assert!(!editor.form.borrow().is_enabled(ReviewSettingsFocus::Add));
    dashboard.handle_key(key(KeyCode::Tab));
    let Mode::Setup(setup) = &mut dashboard.mode else {
        panic!("setup")
    };
    let editor = setup.review_editor.as_mut().unwrap();
    editor.form.get_mut().focus(ReviewSettingsFocus::Remove(1));
    drawn(&mut dashboard, 100, 30);
    let Mode::Setup(setup) = &dashboard.mode else {
        panic!("setup")
    };
    assert_eq!(
        setup
            .review_editor
            .as_ref()
            .unwrap()
            .form
            .borrow()
            .focused(),
        Some(ReviewSettingsFocus::Remove(1))
    );
    dashboard.handle_key(key(KeyCode::Enter));
    drawn(&mut dashboard, 100, 30);
    let Mode::Setup(setup) = &dashboard.mode else {
        panic!("setup")
    };
    assert_eq!(
        setup.review_editor.as_ref().unwrap().review.reviewers.len(),
        2
    );
    let Mode::Setup(setup) = &mut dashboard.mode else {
        panic!("setup")
    };
    let editor = setup.review_editor.as_mut().unwrap();
    editor.review.reviewers[0].model = "manual".into();
    dashboard.handle_key(key(KeyCode::Tab));
    let Mode::Setup(setup) = &mut dashboard.mode else {
        panic!("setup")
    };
    let editor = setup.review_editor.as_mut().unwrap();
    editor
        .form
        .get_mut()
        .focus(ReviewSettingsFocus::Autoconfigure);
    drawn(&mut dashboard, 100, 30);
    dashboard.handle_key(key(KeyCode::Enter));
    let Mode::Setup(setup) = &dashboard.mode else {
        panic!("setup")
    };
    assert_eq!(
        setup.review_editor.as_ref().unwrap().review.reviewers[0].model,
        "gpt-6-terra"
    );
}

#[test]
fn adding_reviewer_during_legacy_upgrade_serializes_the_edit() {
    let mut initial = config();
    initial.review = ReviewConfig::legacy_default();
    let mut dashboard = DashboardState::new(initial, Default::default(), Default::default());
    dashboard.begin_setup();
    dashboard.begin_setup_review();
    let snapshot = catalog(&dashboard.config, &["gpt-6-terra"]);
    dashboard.set_profile_capabilities(snapshot);
    let Mode::Setup(setup) = &mut dashboard.mode else {
        panic!("setup")
    };
    setup
        .review_editor
        .as_mut()
        .unwrap()
        .form
        .get_mut()
        .focus(ReviewSettingsFocus::Add);
    drawn(&mut dashboard, 100, 30);
    dashboard.handle_key(key(KeyCode::Enter));
    let Mode::Setup(setup) = &dashboard.mode else {
        panic!("setup")
    };
    let review = &setup.review_editor.as_ref().unwrap().review;
    assert!(!review.is_legacy());
    assert_eq!(review.reviewers.len(), 1);
    assert!(
        serde_json::to_value(review)
            .unwrap()
            .get("reviewers")
            .is_some()
    );
}

#[test]
fn save_validation_requires_rows_only_for_automatic_trigger() {
    let config = config();
    let mut dialog = ReviewSettingsDialog::new(&config);
    assert!(dialog.can_save());
    dialog.review.trigger = ReviewTrigger::Push;
    assert!(!dialog.can_save());
    assert!(
        ReviewSettingsDialog::validation_error(&dialog.validation_snapshot(), &dialog.review)
            .is_some()
    );
    dialog.review.reviewers.push(SubagentChoice {
        model: "gpt-6-terra".into(),
        effort: None,
    });
    assert!(dialog.can_save());
}

#[test]
fn known_effort_choices_gate_save_but_pending_choices_do_not() {
    let config = config();
    let mut catalog = catalog(&config, &["gpt-6-terra"]);
    let mut dialog = ReviewSettingsDialog::new(&config);
    dialog.review.reviewers.push(SubagentChoice {
        model: "gpt-6-terra".into(),
        effort: Some("xhigh".into()),
    });
    for profile in catalog.profiles.values_mut() {
        profile
            .efforts
            .insert("gpt-6-terra".into(), CapabilityState::Ready(vec![]));
    }
    dialog.update_choices(&config, &catalog);
    assert!(!dialog.can_save());
    assert_eq!(
        ReviewSettingsDialog::validation_error(&dialog.validation_snapshot(), &dialog.review)
            .as_deref(),
        Some("Reviewer 1 effort is unavailable.")
    );
    dialog.review.reviewers[0].effort = None;
    assert!(dialog.can_save());
    dialog.review.reviewers[0].effort = Some("xhigh".into());
    for profile in catalog.profiles.values_mut() {
        profile
            .efforts
            .insert("gpt-6-terra".into(), CapabilityState::Pending);
    }
    dialog.update_choices(&config, &catalog);
    assert!(dialog.can_save());
}

#[test]
fn reviewer_focus_follows_model_effort_remove_order() {
    let mut dashboard = DashboardState::new(config(), Default::default(), Default::default());
    dashboard.begin_setup();
    dashboard.begin_setup_review();
    let snapshot = catalog(&dashboard.config, &["gpt-6-terra", "gpt-6-sol"]);
    dashboard.set_profile_capabilities(snapshot.clone());
    let config = dashboard.config.clone();
    let Mode::Setup(setup) = &mut dashboard.mode else {
        panic!("setup")
    };
    let editor = setup.review_editor.as_mut().unwrap();
    editor.review.reviewers = editor.suggested().unwrap();
    editor.update_choices(&config, &snapshot);
    editor.form.get_mut().focus(ReviewSettingsFocus::Model(0));
    drawn(&mut dashboard, 100, 30);
    for expected in [
        ReviewSettingsFocus::Effort(0),
        ReviewSettingsFocus::Remove(0),
        ReviewSettingsFocus::Model(1),
    ] {
        dashboard.handle_key(key(KeyCode::Tab));
        let Mode::Setup(setup) = &dashboard.mode else {
            panic!("setup")
        };
        assert_eq!(
            setup
                .review_editor
                .as_ref()
                .unwrap()
                .form
                .borrow()
                .focused(),
            Some(expected)
        );
    }
}

#[test]
fn golden_review_settings() {
    let mut dashboard = DashboardState::new(config(), Default::default(), Default::default());
    dashboard.begin_setup();
    dashboard.begin_setup_review();
    dashboard.set_profile_capabilities(catalog(&dashboard.config, &["gpt-6-terra", "gpt-6-sol"]));
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .unwrap();
    let wide = buffer_lines(terminal.backend().buffer()).join("\n");
    let mut narrow_terminal = Terminal::new(TestBackend::new(60, 30)).unwrap();
    narrow_terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .unwrap();
    let narrow = buffer_lines(narrow_terminal.backend().buffer()).join("\n");
    let rendered = format!("=== Wide (100x30) ===\n{wide}\n=== Narrow (60x30) ===\n{narrow}");
    mj_core::golden::assert_golden(
        env!("CARGO_MANIFEST_DIR"),
        "review-settings",
        rendered.trim_end(),
    );
}

#[test]
fn autoconfigure_waits_for_efforts_then_uses_harness_default_for_unsupported_effort() {
    let mut dashboard = DashboardState::new(config(), Default::default(), Default::default());
    dashboard.begin_setup();
    dashboard.begin_setup_review();
    let mut snapshot = catalog(&dashboard.config, &["gpt-6-terra", "gpt-6-sol"]);
    for entry in snapshot.profiles.values_mut() {
        entry.efforts.clear();
    }
    dashboard.set_profile_capabilities(snapshot.clone());
    let Mode::Setup(setup) = &mut dashboard.mode else {
        panic!("setup")
    };
    setup
        .review_editor
        .as_mut()
        .unwrap()
        .form
        .get_mut()
        .focus(ReviewSettingsFocus::Autoconfigure);
    let action = dashboard.handle_key(key(KeyCode::Enter));
    assert!(matches!(
        action,
        DashboardAction::WarmProfileCapabilities { .. }
    ));
    let Mode::Setup(setup) = &dashboard.mode else {
        panic!("setup")
    };
    let editor = setup.review_editor.as_ref().unwrap();
    assert!(editor.pending_autoconfigure.is_some());
    assert!(editor.review.reviewers.is_empty());
    assert!(!editor.can_save());
    for entry in snapshot.profiles.values_mut() {
        entry
            .efforts
            .insert("gpt-6-terra".into(), CapabilityState::Ready(vec![]));
        entry.efforts.insert(
            "gpt-6-sol".into(),
            CapabilityState::Ready(vec![choice("medium")]),
        );
    }
    dashboard.set_profile_capabilities(snapshot.clone());
    let Mode::Setup(setup) = &mut dashboard.mode else {
        panic!("setup")
    };
    let editor = setup.review_editor.as_mut().unwrap();
    editor.update_choices(&dashboard.config, &snapshot);
    assert!(editor.pending_autoconfigure.is_none());
    assert_eq!(editor.review.reviewers[0].effort, None);
    assert_eq!(editor.review.reviewers[1].effort.as_deref(), Some("medium"));
}

#[test]
fn autoconfigure_timeout_finishes_on_clock_tick_without_a_catalog_update() {
    let mut dashboard = DashboardState::new(config(), Default::default(), Default::default());
    dashboard.begin_setup();
    dashboard.begin_setup_review();
    let mut snapshot = catalog(&dashboard.config, &["gpt-6-terra", "gpt-6-sol"]);
    for entry in snapshot.profiles.values_mut() {
        entry.efforts.clear();
    }
    dashboard.set_profile_capabilities(snapshot);
    let Mode::Setup(setup) = &mut dashboard.mode else {
        panic!("setup")
    };
    setup
        .review_editor
        .as_mut()
        .unwrap()
        .form
        .get_mut()
        .focus(ReviewSettingsFocus::Autoconfigure);
    dashboard.handle_key(key(KeyCode::Enter));
    let Mode::Setup(setup) = &mut dashboard.mode else {
        panic!("setup")
    };
    setup
        .review_editor
        .as_mut()
        .unwrap()
        .pending_autoconfigure
        .as_mut()
        .unwrap()
        .1 = Instant::now() - Duration::from_secs(21);
    assert!(dashboard.tick_review_autoconfigure());
    let Mode::Setup(setup) = &dashboard.mode else {
        panic!("setup")
    };
    let editor = setup.review_editor.as_ref().unwrap();
    assert!(!editor.checking_efforts());
    assert_eq!(editor.review.reviewers.len(), 2);
    assert!(
        editor
            .review
            .reviewers
            .iter()
            .all(|choice| choice.effort.is_none())
    );
    assert_eq!(
        editor.effort_notice.as_deref(),
        Some("Some reviewer efforts could not be checked; using defaults.")
    );
}
