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
    dialog.update_choices(
        &config,
        &catalog(&config, &["gpt-6-terra", "gpt-6-sol"]),
        &Default::default(),
    );
    assert!(dialog.review.reviewers.is_empty());
    let suggested = dialog.suggested().unwrap();
    assert_eq!(suggested[0].model, "gpt-6-terra");
    assert_eq!(suggested[0].effort.as_deref(), Some("high"));
    assert!(
        dialog
            .form
            .borrow()
            .is_enabled(ReviewSettingsFocus::Autoconfigure)
    );
    dialog.update_choices(
        &config,
        &catalog(&config, &["gpt-6-terra", "gpt-6-sol"]),
        &Default::default(),
    );
    assert!(dialog.review.reviewers.is_empty());
}

#[test]
fn autoconfigure_skips_models_only_usage_priced_profiles_offer() {
    let config = config();
    let subscription = ["gpt-6.1-terra", "gpt-6.1-sol", "claude-sonnet-4.5"];
    let api = [
        "openrouter/~openai/gpt-terra-latest",
        "openrouter/~openai/gpt-sol-latest",
        "openrouter/~anthropic/claude-sonnet-latest",
    ];
    let mut snapshot = catalog(&config, &subscription);
    // Two usage-priced profiles outnumber the one subscription, so only the
    // billing tier keeps the subscription models.
    let api_entry = catalog(&config, &api)
        .profiles
        .into_values()
        .next()
        .unwrap();
    for id in ["codex-1", "codex-2"] {
        snapshot
            .profiles
            .insert(config.profiles[id].capabilities_key(id), api_entry.clone());
    }
    let suggested = |usage_priced: &[&str]| {
        let mut dialog = ReviewSettingsDialog::new(&config);
        let usage_priced = usage_priced.iter().map(|id| id.to_string()).collect();
        dialog.update_choices(&config, &snapshot, &usage_priced);
        dialog
            .suggested()
            .unwrap()
            .iter()
            .map(|choice| choice.model.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        suggested(&["codex-1", "codex-2"]),
        ["gpt-6.1-terra", "gpt-6.1-sol"]
    );
    // Unknown billing counts as a subscription, so the newer aliases win.
    assert!(suggested(&[])[0].starts_with("openrouter/"));
}

const SUBSCRIPTION_MODELS: [&str; 3] = ["gpt-6.1-terra", "gpt-6.1-sol", "claude-sonnet-4.5"];
const API_MODELS: [&str; 3] = [
    "openrouter/~openai/gpt-terra-latest",
    "openrouter/~openai/gpt-sol-latest",
    "openrouter/~anthropic/claude-sonnet-latest",
];

/// Every profile offers the subscription models except `api`, which offers
/// only newer-looking OpenRouter aliases.
fn billing_snapshot(config: &Config, api: &[&str]) -> ProfileCapabilitiesSnapshot {
    let mut snapshot = catalog(config, &SUBSCRIPTION_MODELS);
    let api_entry = catalog(config, &API_MODELS)
        .profiles
        .into_values()
        .next()
        .unwrap();
    for id in api {
        snapshot
            .profiles
            .insert(config.profiles[*id].capabilities_key(id), api_entry.clone());
    }
    snapshot
}

fn api_quota(
    profile: &str,
    harness: mj_core::config::HarnessKind,
) -> mj_client::quota::ProfileQuota {
    mj_client::quota::ProfileQuota {
        paid_credits: None,
        banked_resets: None,
        profile_id: profile.into(),
        harness,
        windows: Vec::new(),
        extra: Some(mj_client::quota::API_LABEL.into()),
        error: None,
        refreshed_at_epoch_seconds: 0,
        rate_limited_until_epoch_seconds: None,
    }
}

fn setup_review_models(dashboard: &DashboardState) -> Vec<String> {
    let Mode::Setup(setup) = &dashboard.mode else {
        panic!("setup")
    };
    setup
        .review_editor
        .as_ref()
        .unwrap()
        .review
        .reviewers
        .iter()
        .map(|choice| choice.model.clone())
        .collect()
}

fn press_autoconfigure(dashboard: &mut DashboardState) {
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
}

#[test]
fn autoconfigure_button_uses_the_quota_reports_present_when_pressed() {
    let mut dashboard = DashboardState::new(config(), Default::default(), Default::default());
    dashboard.begin_setup();
    dashboard.begin_setup_review();
    // The suggestion is computed before any quota report: both OpenRouter
    // profiles count as subscriptions and their newer aliases win.
    let snapshot = billing_snapshot(&dashboard.config, &["codex-1", "codex-2"]);
    dashboard.set_profile_capabilities(snapshot);
    let Mode::Setup(setup) = &dashboard.mode else {
        panic!("setup")
    };
    assert!(
        setup.review_editor.as_ref().unwrap().suggested().unwrap()[0]
            .model
            .starts_with("openrouter/")
    );
    // The reports arrive without any event that refreshes the dialog.
    for id in ["codex-1", "codex-2"] {
        dashboard.quotas.insert(
            id.into(),
            api_quota(id, mj_core::config::HarnessKind::Codex),
        );
    }
    press_autoconfigure(&mut dashboard);
    assert_eq!(setup_review_models(&dashboard), SUBSCRIPTION_MODELS[..2]);
}

#[test]
fn quota_updates_refresh_the_open_dialogs_suggestion() {
    let mut dashboard = DashboardState::new(config(), Default::default(), Default::default());
    dashboard.begin_setup();
    dashboard.begin_setup_review();
    let snapshot = billing_snapshot(&dashboard.config, &["codex-1", "codex-2"]);
    dashboard.set_profile_capabilities(snapshot);
    let suggested = |dashboard: &DashboardState| {
        let Mode::Setup(setup) = &dashboard.mode else {
            panic!("setup")
        };
        setup.review_editor.as_ref().unwrap().suggested().unwrap()[0]
            .model
            .clone()
    };
    assert!(suggested(&dashboard).starts_with("openrouter/"));
    dashboard.apply_quota(api_quota("codex-1", mj_core::config::HarnessKind::Codex));
    dashboard.apply_quota(api_quota("codex-2", mj_core::config::HarnessKind::Codex));
    assert_eq!(suggested(&dashboard), "gpt-6.1-terra");
}

#[test]
fn an_opencode_profile_without_a_report_is_usage_priced() {
    let mut config = config();
    config.profiles.insert("openrouter".into(), {
        let mut profile = config.profiles["codex-1"].clone();
        profile.kind = mj_core::config::HarnessKind::OpenCode;
        profile.home = "/profiles/openrouter/opencode".into();
        profile
    });
    let mut dashboard = DashboardState::new(config, Default::default(), Default::default());
    assert_eq!(
        dashboard.usage_priced_profiles(&dashboard.config),
        std::collections::BTreeSet::from(["openrouter".to_owned()])
    );
    dashboard.begin_setup();
    dashboard.begin_setup_review();
    let snapshot = billing_snapshot(&dashboard.config, &["openrouter"]);
    dashboard.set_profile_capabilities(snapshot);
    press_autoconfigure(&mut dashboard);
    assert_eq!(setup_review_models(&dashboard), SUBSCRIPTION_MODELS[..2]);
}

#[test]
fn opening_with_reviewers_preserves_them_when_catalog_arrives() {
    let mut config = config();
    config.review.reviewers = vec![SubagentChoice {
        model: "custom-model".into(),
        effort: Some("high".into()),
    }];
    let mut dialog = ReviewSettingsDialog::new(&config);
    dialog.update_choices(
        &config,
        &catalog(&config, &["gpt-6-terra", "gpt-6-sol"]),
        &Default::default(),
    );
    assert_eq!(dialog.review.reviewers, config.review.reviewers);
}

#[test]
fn autoconfigure_disabled_without_supported_models() {
    let config = config();
    let mut dialog = ReviewSettingsDialog::new(&config);
    dialog.update_choices(
        &config,
        &catalog(&config, &["other-model"]),
        &Default::default(),
    );
    assert!(dialog.review.reviewers.is_empty());
    assert!(
        !dialog
            .form
            .borrow()
            .is_enabled(ReviewSettingsFocus::Autoconfigure)
    );
    assert!(dialog.can_save()); // No reviewers means review is off.
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
    dialog.update_choices(&config, &catalog, &Default::default());
    dialog.review.reviewers = dialog.suggested().unwrap();
    dialog.update_choices(&config, &catalog, &Default::default());
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
    editor.update_choices(&config, &snapshot, &Default::default());
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
    dialog.update_choices(&config, &catalog, &Default::default());
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
    dialog.update_choices(&config, &catalog, &Default::default());
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
    editor.update_choices(&config, &snapshot, &Default::default());
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
    dashboard.set_profile_capabilities(catalog(
        &dashboard.config,
        &[
            "gpt-6-terra",
            "gpt-6-sol",
            "claude-sonnet-5-5",
            "claude-haiku-5-5",
        ],
    ));
    press_autoconfigure(&mut dashboard);
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
    let mut anthropic = DashboardState::new(config(), Default::default(), Default::default());
    anthropic.begin_setup();
    anthropic.begin_setup_review();
    anthropic.set_profile_capabilities(catalog(
        &anthropic.config,
        &["claude-sonnet-5-5", "claude-haiku-5-5"],
    ));
    press_autoconfigure(&mut anthropic);
    terminal
        .draw(|frame| crate::render::render(frame, &mut anthropic))
        .unwrap();
    let anthropic = buffer_lines(terminal.backend().buffer()).join("\n");
    let rendered = format!(
        "=== Wide (100x30) ===\n{wide}\n=== Narrow (60x30) ===\n{narrow}\n=== Anthropic (100x30) ===\n{anthropic}"
    );
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
    editor.update_choices(&dashboard.config, &snapshot, &Default::default());
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
        Some(
            "Reviewer effort discovery failed: claude-1 (timed out); codex-1 (timed out); codex-2 (timed out); using defaults."
        )
    );
}

#[test]
fn unchanged_legacy_review_remains_valid_during_unrelated_save() {
    let mut config = config();
    config.review = ReviewConfig::legacy_default();
    let dialog = ReviewSettingsDialog::new(&config);
    assert!(dialog.can_save());
    assert!(
        ReviewSettingsDialog::validation_error(&dialog.validation_snapshot(), &dialog.review)
            .is_none()
    );
}

#[test]
fn failed_effort_discovery_is_reported_when_autoconfigure_times_out() {
    let config = config();
    let mut snapshot = catalog(&config, &["gpt-6-terra"]);
    for profile in snapshot.profiles.values_mut() {
        profile.efforts.insert(
            "gpt-6-terra".into(),
            CapabilityState::Failed("probe failed".into()),
        );
    }
    let mut dialog = ReviewSettingsDialog::new(&config);
    dialog.pending_autoconfigure = Some((
        vec![SubagentChoice {
            model: "gpt-6-terra".into(),
            effort: Some("high".into()),
        }],
        Instant::now() - Duration::from_secs(21),
    ));
    dialog.update_choices(&config, &snapshot, &Default::default());
    assert_eq!(dialog.review.reviewers[0].effort, None);
    assert!(
        dialog
            .effort_notice
            .as_deref()
            .unwrap()
            .contains("probe failed")
    );
}

#[test]
fn partial_discovery_does_not_reject_configured_reviewer_on_save() {
    let mut config = config();
    config.review.reviewers = vec![SubagentChoice {
        model: "custom-model".into(),
        effort: Some("high".into()),
    }];
    let mut snapshot = catalog(&config, &[]);
    for profile in snapshot.profiles.values_mut() {
        profile.choices = CapabilityState::Failed("probe failed".into());
    }
    let mut dialog = ReviewSettingsDialog::new(&config);
    dialog.update_choices(&config, &snapshot, &Default::default());
    assert!(dialog.can_save());
    assert!(
        ReviewSettingsDialog::validation_error(&dialog.validation_snapshot(), &dialog.review)
            .is_none()
    );
}

#[test]
fn autoconfigure_warms_newly_selected_model_within_existing_wait() {
    let mut dashboard = DashboardState::new(config(), Default::default(), Default::default());
    dashboard.begin_setup();
    dashboard.begin_setup_review();
    let initial = catalog(&dashboard.config, &["claude-sonnet-5-5"]);
    dashboard.set_profile_capabilities(initial);
    let Mode::Setup(setup) = &mut dashboard.mode else {
        panic!("setup")
    };
    let editor = setup.review_editor.as_mut().unwrap();
    editor.pending_autoconfigure = Some((
        vec![SubagentChoice {
            model: "claude-sonnet-5-5".into(),
            effort: Some("xhigh".into()),
        }],
        Instant::now(),
    ));
    let mut revised = catalog(&dashboard.config, &["gpt-6-terra", "gpt-6-sol"]);
    for entry in revised.profiles.values_mut() {
        entry.efforts.remove("gpt-6-terra");
        entry.efforts.remove("gpt-6-sol");
    }
    dashboard.set_profile_capabilities(revised);
    let Mode::Setup(setup) = &dashboard.mode else {
        panic!("setup")
    };
    assert!(setup.review_editor.as_ref().unwrap().checking_efforts());
    assert!(matches!(
        dashboard.take_profile_hydration(),
        Some(DashboardAction::WarmProfileCapabilities { .. })
    ));
}

#[test]
fn legacy_draft_autoconfigure_warms_the_recomputed_models() {
    let mut legacy = config();
    legacy.review = mj_core::config::ReviewConfig::legacy_default();
    let mut dashboard = DashboardState::new(legacy, Default::default(), Default::default());
    dashboard.begin_setup();
    dashboard.begin_setup_review();
    dashboard.set_profile_capabilities(catalog(&dashboard.config, &["claude-sonnet-5-5"]));
    let Mode::Setup(setup) = &mut dashboard.mode else {
        panic!("setup")
    };
    setup.review_editor.as_mut().unwrap().pending_autoconfigure = Some((
        vec![SubagentChoice {
            model: "claude-sonnet-5-5".into(),
            effort: Some("xhigh".into()),
        }],
        Instant::now(),
    ));
    let mut revised = catalog(&dashboard.config, &["gpt-6-terra", "gpt-6-sol"]);
    for entry in revised.profiles.values_mut() {
        entry.efforts.remove("gpt-6-terra");
        entry.efforts.remove("gpt-6-sol");
    }
    dashboard.set_profile_capabilities(revised);
    let Some(DashboardAction::WarmProfileCapabilities { config, .. }) =
        dashboard.take_profile_hydration()
    else {
        panic!("expected a warm request")
    };
    let warmed: Config = serde_json::from_str(&config).unwrap();
    assert!(!warmed.review.is_legacy());
    assert!(
        warmed
            .review
            .reviewers
            .iter()
            .any(|reviewer| reviewer.model == "gpt-6-terra"),
        "{config}"
    );
}

#[test]
fn autoconfigure_ignores_a_failed_profile_that_does_not_offer_the_model() {
    let mut config = config();
    let mut snapshot = catalog(&config, &["gpt-6-terra", "gpt-6-sol"]);
    // Medium is the closest offered effort below Terra's requested high.
    for entry in snapshot.profiles.values_mut() {
        for model in ["gpt-6-terra", "gpt-6-sol"] {
            entry.efforts.insert(
                model.into(),
                CapabilityState::Ready(vec![choice("low"), choice("medium")]),
            );
        }
    }
    // An unrelated profile whose discovery failed.
    let (id, profile) = config
        .profiles
        .iter()
        .next()
        .map(|(id, profile)| (id.clone(), profile.clone()))
        .unwrap();
    let mut failed = profile;
    failed.home = "/profiles/failed".into();
    let failed_id = format!("{id}-failed");
    snapshot.profiles.insert(
        failed.capabilities_key(&failed_id),
        ProfileCapabilities {
            choices: CapabilityState::Failed("login expired".into()),
            efforts: Default::default(),
        },
    );
    config.profiles.insert(failed_id, failed);
    let mut dialog = ReviewSettingsDialog::new(&config);
    dialog.pending_autoconfigure = Some((Vec::new(), Instant::now()));
    dialog.update_choices(&config, &snapshot, &Default::default());
    assert!(dialog.pending_autoconfigure.is_none());
    assert!(!dialog.review.reviewers.is_empty());
    for reviewer in &dialog.review.reviewers {
        assert!(
            matches!(reviewer.effort.as_deref(), Some("low" | "medium")),
            "{reviewer:?}"
        );
    }
    assert!(
        dialog
            .review
            .reviewers
            .iter()
            .any(|reviewer| reviewer.effort.as_deref() == Some("medium"))
    );
    let notice = dialog.effort_notice.as_deref().unwrap();
    assert!(notice.contains("Adjusted reviewer efforts"), "{notice}");
    assert!(!notice.contains("discovery failed"), "{notice}");
    assert!(!notice.contains("timed out"), "{notice}");
}
