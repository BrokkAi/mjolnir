use super::*;
use crate::actions::CommandId;
use crate::test_support::{
    buffer_lines, config, dashboard_with_session, key, open_palette, running_session,
};
use crossterm::event::KeyCode;
use ratatui::{Terminal, backend::TestBackend};

fn open(dashboard: &mut DashboardState) -> DashboardAction {
    dashboard.begin_review_settings()
}

fn dialog(dashboard: &DashboardState) -> &ReviewSettingsDialog {
    let Mode::Setup(setup) = &dashboard.mode else {
        panic!("expected review settings dialog")
    };
    setup.review_editor.as_ref().expect("review child")
}

fn choice(value: &str) -> SessionConfigChoice {
    SessionConfigChoice {
        value: value.to_owned(),
        name: value.to_owned(),
        description: None,
    }
}

fn choose_next(dashboard: &mut DashboardState) -> DashboardAction {
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Down)),
        DashboardAction::None
    );
    dashboard.handle_key(key(KeyCode::Enter))
}

fn choose_first(dashboard: &mut DashboardState) -> DashboardAction {
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Home)),
        DashboardAction::None
    );
    dashboard.handle_key(key(KeyCode::Enter))
}

fn available(
    model_choices: &[&str],
    effort_choices: &[&str],
    effort_known: bool,
) -> ReviewSettingsDiscoveryResult {
    ReviewSettingsDiscoveryResult::Available {
        choices: ReviewSettingsChoices {
            model_choices: model_choices.iter().map(|value| choice(value)).collect(),
            effort_choices: effort_choices.iter().map(|value| choice(value)).collect(),
            effort_capabilities_discovered: effort_known,
        },
        cleanup_warning: None,
    }
}

#[test]
fn review_selectors_render_as_comboboxes_and_escape_closes_only_the_popup() {
    let mut dashboard = dashboard_with_session(running_session());
    open(&mut dashboard);
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|frame| crate::render::render(frame, &mut dashboard))
        .unwrap();
    let text = buffer_lines(terminal.backend().buffer()).join("\n");
    assert!(text.matches(ComboBox::GLYPH).count() >= 4, "{text}");

    while dialog(&dashboard).focused() != ReviewSettingsFocus::Tier {
        dashboard.handle_key(key(KeyCode::Tab));
    }
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert!(dialog(&dashboard).combo.is_open(ReviewSettingsFocus::Tier));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Esc)),
        DashboardAction::None
    );
    assert!(matches!(dashboard.mode, Mode::Setup(_)));
    assert!(dialog(&dashboard).combo.open_id().is_none());
}

#[test]
fn progress_choices_are_cached_before_cleanup_and_stale_replies_are_ignored() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.config.review.profile = Some("codex-1".into());
    dashboard.config.review.enabled = true;
    let DashboardAction::DiscoverReviewSettings {
        generation,
        profile_id,
        model,
    } = open(&mut dashboard)
    else {
        panic!("configured profile must start discovery")
    };
    assert!(dashboard.needs_fast_tick());
    assert!(dashboard.apply_review_settings_choices(
        generation,
        &profile_id,
        model.as_deref(),
        ReviewSettingsChoices::default(),
    ));
    assert!(dialog(&dashboard).model_choices_discovered);
    assert!(dialog(&dashboard).probing);
    assert!(
        dashboard
            .review_settings_choices
            .contains_key(&(profile_id.clone(), model.clone()))
    );
    dashboard.handle_key(key(KeyCode::Esc));
    assert!(!dashboard.needs_fast_tick());
    open(&mut dashboard);
    assert!(!dashboard.apply_review_settings_choices(
        generation,
        &profile_id,
        model.as_deref(),
        ReviewSettingsChoices::default(),
    ));
    assert!(dialog(&dashboard).model_choices_discovered);
}

#[test]
fn review_settings_is_available_through_setup_without_a_selected_session() {
    let mut dashboard = DashboardState::new(
        config(),
        mj_core::state::State::default(),
        Default::default(),
    );
    let action = open(&mut dashboard);
    assert!(matches!(action, DashboardAction::None));
    assert!(matches!(dashboard.mode, Mode::Setup(_)));
    assert_eq!(dashboard.selected_session_id(), None);

    let mut dashboard = DashboardState::new(
        config(),
        mj_core::state::State::default(),
        Default::default(),
    );
    open_palette(&mut dashboard);
    let Mode::Palette(palette) = &dashboard.mode else {
        panic!("the palette chord should open the command palette")
    };
    assert!(
        palette
            .entries
            .iter()
            .any(|entry| entry.id == CommandId::OpenConfig)
    );
}

#[test]
fn clearing_the_profile_cancels_discovery_without_closing_the_draft() {
    let mut dashboard = dashboard_with_session(running_session());
    open(&mut dashboard);
    dashboard.handle_key(key(KeyCode::Char(' ')));
    dashboard.handle_key(key(KeyCode::Tab));
    dashboard.handle_key(key(KeyCode::Tab));
    assert!(matches!(
        choose_next(&mut dashboard),
        DashboardAction::DiscoverReviewSettings { .. }
    ));
    assert!(matches!(
        choose_first(&mut dashboard),
        DashboardAction::CancelReviewSettingsDiscovery
    ));
    assert!(dialog(&dashboard).review.profile.is_none());
    assert!(dialog(&dashboard).review.enabled);
    assert!(dialog(&dashboard).can_save());
    // Esc returns to Settings with the change still in its draft.
    dashboard.handle_key(key(KeyCode::Esc));
    assert!(!dashboard.dialog_confirmation_open());
    let Mode::Setup(setup) = &dashboard.mode else {
        panic!("Esc returns to Settings")
    };
    assert!(setup.review_editor.is_none());
    assert!(setup.is_dirty(), "the enabled review stays in the draft");
}

#[test]
fn edit_and_save_action_contains_only_global_review_values() {
    let mut dashboard = dashboard_with_session(running_session());
    let _ = open(&mut dashboard);
    // Enabled -> Tier -> Profile, then choose the first configured profile.
    dashboard.handle_key(key(KeyCode::Tab));
    dashboard.handle_key(key(KeyCode::Tab));
    let probe = choose_next(&mut dashboard);
    assert!(matches!(
        probe,
        DashboardAction::DiscoverReviewSettings { .. }
    ));

    while dialog(&dashboard).focused() != ReviewSettingsFocus::Tier {
        dashboard.handle_key(key(KeyCode::Tab));
    }
    choose_next(&mut dashboard);
    while dialog(&dashboard).focused() != ReviewSettingsFocus::Save {
        dashboard.handle_key(key(KeyCode::Tab));
    }
    let action = dashboard.handle_key(key(KeyCode::Enter));
    assert!(matches!(action, DashboardAction::SaveSetup { .. }));
    assert!(matches!(dashboard.mode, Mode::Setup(_)));
}

#[test]
fn stale_discovery_does_not_replace_choices_after_model_change() {
    let mut config = config();
    config.review.profile = Some("codex-1".into());
    let mut dashboard =
        DashboardState::new(config, mj_core::state::State::default(), Default::default());
    let initial = open(&mut dashboard);
    let DashboardAction::DiscoverReviewSettings {
        generation,
        profile_id,
        model,
    } = initial
    else {
        panic!("expected initial probe")
    };
    dashboard.apply_review_settings_discovery(
        generation,
        &profile_id,
        model.as_deref(),
        Ok(ReviewSettingsDiscoveryResult::Available {
            choices: ReviewSettingsChoices {
                model_choices: vec![SessionConfigChoice {
                    value: "model-a".into(),
                    name: "Model A".into(),
                    description: None,
                }],
                effort_choices: vec![],
                effort_capabilities_discovered: false,
            },
            cleanup_warning: None,
        }),
    );
    while dialog(&dashboard).focused() != ReviewSettingsFocus::Model {
        dashboard.handle_key(key(KeyCode::Tab));
    }
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Down)),
        DashboardAction::None,
        "previewing a model must not start discovery"
    );
    assert_eq!(dialog(&dashboard).generation, generation);
    let model_action = dashboard.handle_key(key(KeyCode::Enter));
    let DashboardAction::DiscoverReviewSettings {
        generation: newer,
        profile_id,
        model,
    } = model_action
    else {
        panic!("expected model probe")
    };
    assert!(newer > generation);
    assert!(dialog(&dashboard).probing);
    assert_eq!(dialog(&dashboard).model_choices[0].value, "model-a");
    assert!(dialog(&dashboard).effort_choices.is_empty());
    dashboard.apply_review_settings_discovery(
        generation,
        &profile_id,
        None,
        Ok(ReviewSettingsDiscoveryResult::Available {
            choices: ReviewSettingsChoices {
                model_choices: vec![SessionConfigChoice {
                    value: "stale".into(),
                    name: "Stale".into(),
                    description: None,
                }],
                effort_choices: vec![],
                effort_capabilities_discovered: false,
            },
            cleanup_warning: None,
        }),
    );
    assert_eq!(dialog(&dashboard).model_choices[0].value, "model-a");
    assert!(dialog(&dashboard).probing);
    dashboard.apply_review_settings_discovery(
        newer,
        &profile_id,
        model.as_deref(),
        Ok(ReviewSettingsDiscoveryResult::Available {
            choices: ReviewSettingsChoices::default(),
            cleanup_warning: None,
        }),
    );
    assert!(!dialog(&dashboard).probing);
}

#[test]
fn selecting_the_current_value_does_not_restart_pending_discovery() {
    let mut config = config();
    config.review.profile = Some("codex-1".into());
    let mut dashboard =
        DashboardState::new(config, mj_core::state::State::default(), Default::default());
    let initial = open(&mut dashboard);
    assert!(matches!(
        initial,
        DashboardAction::DiscoverReviewSettings { .. }
    ));
    let generation = dialog(&dashboard).generation;
    while dialog(&dashboard).focused() != ReviewSettingsFocus::Model {
        dashboard.handle_key(key(KeyCode::Tab));
    }
    assert_eq!(choose_first(&mut dashboard), DashboardAction::None);
    assert!(dialog(&dashboard).probing);
    assert_eq!(dialog(&dashboard).generation, generation);
}

#[test]
fn effort_change_does_not_restart_discovery() {
    let mut config = config();
    config.review.profile = Some("codex-1".into());
    let mut dashboard =
        DashboardState::new(config, mj_core::state::State::default(), Default::default());
    let DashboardAction::DiscoverReviewSettings { generation, .. } = open(&mut dashboard) else {
        panic!("expected initial probe")
    };
    dashboard.apply_review_settings_discovery(
        generation,
        "codex-1",
        None,
        Ok(ReviewSettingsDiscoveryResult::Available {
            choices: ReviewSettingsChoices {
                model_choices: vec![],
                effort_choices: vec![SessionConfigChoice {
                    value: "low".into(),
                    name: "Low".into(),
                    description: None,
                }],
                effort_capabilities_discovered: true,
            },
            cleanup_warning: None,
        }),
    );
    while dialog(&dashboard).focused() != ReviewSettingsFocus::Effort {
        dashboard.handle_key(key(KeyCode::Tab));
    }
    let action = choose_next(&mut dashboard);
    assert_eq!(action, DashboardAction::None);
    assert!(!dialog(&dashboard).probing);
}

#[test]
fn selectors_show_unverified_until_capabilities_are_discovered() {
    assert_eq!(
        ReviewSettingsDialog::value_label(Some("opus"), &[], false),
        "opus (unverified)"
    );
    assert_eq!(
        ReviewSettingsDialog::value_label(Some("opus"), &[], true),
        "opus (unavailable)"
    );
}

#[test]
fn closing_and_reopening_uses_cached_choices() {
    let mut config = config();
    config.review.profile = Some("codex-1".into());
    let mut dashboard =
        DashboardState::new(config, mj_core::state::State::default(), Default::default());
    let DashboardAction::DiscoverReviewSettings {
        generation,
        profile_id,
        model,
    } = open(&mut dashboard)
    else {
        panic!("expected initial probe")
    };
    if let Mode::Setup(setup) = &mut dashboard.mode {
        setup
            .review_editor
            .as_mut()
            .expect("review editor")
            .form
            .get_mut()
            .focus(ReviewSettingsFocus::Back);
    } else {
        panic!("setup remains open after discovery");
    }
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::CancelReviewSettingsDiscovery
    );
    dashboard.cancel_modal();
    assert!(!dashboard.modal_open());

    assert!(!dashboard.apply_review_settings_discovery(
        generation,
        &profile_id,
        model.as_deref(),
        Ok(ReviewSettingsDiscoveryResult::Available {
            choices: ReviewSettingsChoices {
                model_choices: vec![SessionConfigChoice {
                    value: "old".into(),
                    name: "Old".into(),
                    description: None,
                }],
                effort_choices: vec![],
                effort_capabilities_discovered: false,
            },
            cleanup_warning: None,
        }),
    ));
    let DashboardAction::DiscoverReviewSettings {
        generation: reopened,
        profile_id,
        model,
    } = open(&mut dashboard)
    else {
        panic!("expected reopened discovery")
    };
    dashboard.apply_review_settings_discovery(
        reopened,
        &profile_id,
        model.as_deref(),
        Ok(ReviewSettingsDiscoveryResult::Available {
            choices: ReviewSettingsChoices {
                model_choices: vec![SessionConfigChoice {
                    value: "old".into(),
                    name: "Old".into(),
                    description: None,
                }],
                effort_choices: vec![],
                effort_capabilities_discovered: false,
            },
            cleanup_warning: None,
        }),
    );
    dashboard.cancel_modal();
    let reopened = open(&mut dashboard);
    assert_eq!(reopened, DashboardAction::None);
    assert!(dialog(&dashboard).model_choices_discovered);
    assert_eq!(dialog(&dashboard).model_choices[0].value, "old");
}

#[test]
fn refresh_clears_only_the_profile_cache_and_retains_current_choices() {
    let mut config = config();
    config.review.profile = Some("codex-1".into());
    let mut dashboard =
        DashboardState::new(config, mj_core::state::State::default(), Default::default());
    dashboard.review_settings_choices.insert(
        ("codex-1".into(), None),
        ReviewSettingsChoices {
            model_choices: vec![choice("tiny")],
            effort_choices: vec![choice("high")],
            effort_capabilities_discovered: true,
        },
    );
    assert_eq!(open(&mut dashboard), DashboardAction::None);
    assert_eq!(dialog(&dashboard).model_choices[0].value, "tiny");
    while dialog(&dashboard).focused() != ReviewSettingsFocus::Refresh {
        dashboard.handle_key(key(KeyCode::Tab));
    }
    let action = dashboard.handle_key(key(KeyCode::Enter));
    assert!(matches!(
        action,
        DashboardAction::DiscoverReviewSettings { model: None, .. }
    ));
    assert!(
        !dashboard
            .review_settings_choices
            .contains_key(&("codex-1".into(), None))
    );
    assert_eq!(dialog(&dashboard).model_choices[0].value, "tiny");
    assert!(dialog(&dashboard).probing);
    assert!(dialog(&dashboard).choices_loading);
}

#[test]
fn profile_definition_changes_invalidate_cache_but_review_edits_do_not() {
    let mut dashboard = DashboardState::new(
        config(),
        mj_core::state::State::default(),
        Default::default(),
    );
    let key = ("codex-1".to_owned(), None);
    dashboard
        .review_settings_choices
        .insert(key.clone(), ReviewSettingsChoices::default());
    let mut review_edit = dashboard.config.clone();
    review_edit.review.enabled = true;
    dashboard.set_config(review_edit);
    assert!(dashboard.review_settings_choices.contains_key(&key));

    let mut profile_edit = dashboard.config.clone();
    profile_edit.profiles.get_mut("codex-1").unwrap().home = "/changed".into();
    dashboard.set_config(profile_edit);
    assert!(!dashboard.review_settings_choices.contains_key(&key));
}

#[test]
fn matching_discovery_replies_apply_through_help() {
    let mut config = config();
    config.review.profile = Some("codex-1".into());
    let mut dashboard =
        DashboardState::new(config, mj_core::state::State::default(), Default::default());
    let DashboardAction::DiscoverReviewSettings {
        generation,
        profile_id,
        model,
    } = open(&mut dashboard)
    else {
        panic!("expected discovery")
    };
    dashboard.begin_help();
    assert!(dashboard.apply_review_settings_choices(
        generation,
        &profile_id,
        model.as_deref(),
        ReviewSettingsChoices {
            model_choices: vec![choice("tiny")],
            effort_choices: vec![],
            effort_capabilities_discovered: false,
        },
    ));
    let Mode::Help(overlay) = &dashboard.mode else {
        panic!("expected help")
    };
    let Mode::Setup(setup) = overlay.return_to.as_ref() else {
        panic!("help must cover review settings")
    };
    let dialog = setup.review_editor.as_ref().expect("review child");
    assert!(dialog.model_choices_discovered);
    assert!(dialog.probing);
    assert!(
        dashboard
            .review_settings_choices
            .contains_key(&(profile_id, model))
    );
}

#[test]
fn final_cleanup_warning_keeps_choices_and_zero_effort_is_known() {
    let mut config = config();
    config.review.profile = Some("codex-1".into());
    let mut dashboard =
        DashboardState::new(config, mj_core::state::State::default(), Default::default());
    let DashboardAction::DiscoverReviewSettings {
        generation,
        profile_id,
        model,
    } = open(&mut dashboard)
    else {
        panic!("expected discovery")
    };
    let mut final_result = available(&["tiny"], &[], true);
    if let ReviewSettingsDiscoveryResult::Available {
        cleanup_warning, ..
    } = &mut final_result
    {
        *cleanup_warning = Some("worker cleanup timed out".into());
    }
    assert!(dashboard.apply_review_settings_discovery(
        generation,
        &profile_id,
        model.as_deref(),
        Ok(final_result),
    ));
    assert!(!dialog(&dashboard).probing);
    assert_eq!(
        dialog(&dashboard).cleanup_warning.as_deref(),
        Some("worker cleanup timed out")
    );
    assert!(dialog(&dashboard).model_choices_discovered);
    assert!(dialog(&dashboard).effort_capabilities_discovered);
    assert!(dialog(&dashboard).can_save());
}

#[test]
fn known_unsupported_values_disable_save_but_unknown_discovery_does_not() {
    let mut config = config();
    config.review.profile = Some("codex-1".into());
    config.review.enabled = true;
    config.review.model = Some("missing".into());
    config.review.effort = Some("missing".into());
    let mut dashboard =
        DashboardState::new(config, mj_core::state::State::default(), Default::default());
    let DashboardAction::DiscoverReviewSettings {
        generation,
        profile_id,
        model,
    } = open(&mut dashboard)
    else {
        panic!("expected discovery")
    };
    assert!(dialog(&dashboard).can_save());
    assert!(dashboard.apply_review_settings_discovery(
        generation,
        &profile_id,
        model.as_deref(),
        Ok(available(&["tiny"], &["high"], true)),
    ));
    assert!(!dialog(&dashboard).can_save());
    if let Mode::Setup(setup) = &mut dashboard.mode {
        setup
            .review_editor
            .as_mut()
            .expect("review editor")
            .form
            .get_mut()
            .focus(ReviewSettingsFocus::Back);
    } else {
        panic!("setup remains open after discovery");
    }
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::CancelReviewSettingsDiscovery
    );
    let action = dashboard.handle_key(crossterm::event::KeyEvent::new(
        KeyCode::Char('s'),
        crossterm::event::KeyModifiers::CONTROL,
    ));
    assert_eq!(action, DashboardAction::None);
    let Mode::Setup(setup) = &dashboard.mode else {
        panic!("setup remains open after rejected save")
    };
    assert!(
        setup
            .notice
            .as_deref()
            .is_some_and(|notice| notice.contains("unavailable"))
    );
}

#[test]
fn save_is_local_while_loading_unavailable_or_failed() {
    let mut config = config();
    config.review.profile = Some("codex-1".into());
    config.review.enabled = true;
    let mut dashboard =
        DashboardState::new(config, mj_core::state::State::default(), Default::default());
    assert!(matches!(
        open(&mut dashboard),
        DashboardAction::DiscoverReviewSettings { .. }
    ));
    while dialog(&dashboard).focused() != ReviewSettingsFocus::Save {
        dashboard.handle_key(key(KeyCode::Tab));
    }
    assert!(matches!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::SaveSetup { .. }
    ));
    dashboard.cancel_modal();

    let DashboardAction::DiscoverReviewSettings {
        generation,
        profile_id,
        model,
    } = open(&mut dashboard)
    else {
        panic!("expected rediscovery")
    };
    assert!(dashboard.apply_review_settings_discovery(
        generation,
        &profile_id,
        model.as_deref(),
        Ok(ReviewSettingsDiscoveryResult::Unavailable),
    ));
    assert!(dialog(&dashboard).can_save());
    dashboard.cancel_modal();

    let DashboardAction::DiscoverReviewSettings {
        generation,
        profile_id,
        model,
    } = open(&mut dashboard)
    else {
        panic!("expected rediscovery")
    };
    assert!(dashboard.apply_review_settings_discovery(
        generation,
        &profile_id,
        model.as_deref(),
        Err("offline".to_owned()),
    ));
    assert!(dialog(&dashboard).can_save());
}

#[test]
fn auto_is_explicit_saveable_and_disables_manual_model_overrides() {
    let mut dashboard = dashboard_with_session(running_session());
    open(&mut dashboard);
    let editor = dialog(&dashboard);
    let selectors = editor.selectors();
    assert_eq!(
        selectors
            .iter()
            .find(|(id, _, _, _)| *id == ReviewSettingsFocus::Profile)
            .unwrap()
            .2[0],
        "Auto"
    );
    assert!(editor.can_save());
    assert!(!editor.form.borrow().is_enabled(ReviewSettingsFocus::Model));
    assert!(!editor.form.borrow().is_enabled(ReviewSettingsFocus::Effort));
}
