use super::*;
use crate::actions::CommandId;
use crate::test_support::{
    buffer_lines, chord, config, dashboard_with_session, drawn, key, mouse_at, open_palette, point,
    running_session,
};
use crossterm::event::{KeyCode, MouseButton, MouseEventKind};
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

fn rendered(dashboard: &mut DashboardState, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal
        .draw(|frame| crate::render::render(frame, dashboard))
        .expect("draw review settings");
    buffer_lines(terminal.backend().buffer()).join("\n")
}

fn append_golden_state(output: &mut String, label: &str, width: u16, height: u16, surface: &str) {
    use std::fmt::Write as _;

    if !output.is_empty() {
        output.push('\n');
    }
    writeln!(output, "=== {label} ({width}x{height}) ===").expect("write state header");
    output.push_str(surface);
    output.push('\n');
}

fn open_review_from_setup(dashboard: &mut DashboardState) -> DashboardAction {
    let _ = chord(dashboard, CommandId::OpenConfig);
    let lines = drawn(dashboard, 100, 30);
    let code_review = point(&lines, "Code Review");
    for _ in 0..2 {
        dashboard.handle_mouse(mouse_at(
            MouseEventKind::Down(MouseButton::Left),
            code_review,
        ));
        dashboard.handle_mouse(mouse_at(MouseEventKind::Up(MouseButton::Left), code_review));
    }
    DashboardAction::None
}

#[test]
fn golden_review_settings() {
    use std::fmt::Write as _;

    let mut output = String::new();

    let mut no_session = DashboardState::new(
        config(),
        mj_core::state::State::default(),
        Default::default(),
    );
    let action = open_review_from_setup(&mut no_session);
    append_golden_state(
        &mut output,
        "Code Review opens without a selected session",
        100,
        30,
        &rendered(&mut no_session, 100, 30),
    );
    writeln!(output, "action: {action:?}").expect("write entry action");
    writeln!(
        output,
        "selected session: {:?}",
        no_session.selected_session_id()
    )
    .expect("write selected session");

    let mut palette = DashboardState::new(
        config(),
        mj_core::state::State::default(),
        Default::default(),
    );
    open_palette(&mut palette);
    let has_setup = match &palette.mode {
        Mode::Palette(palette) => palette
            .entries
            .iter()
            .any(|entry| entry.id == CommandId::OpenConfig),
        _ => false,
    };
    append_golden_state(
        &mut output,
        "command palette exposes Setup without a selected session",
        100,
        30,
        &rendered(&mut palette, 100, 30),
    );
    writeln!(output, "Setup command available: {has_setup}").expect("write command availability");

    let mut dashboard = dashboard_with_session(running_session());
    let entry_action = open_review_from_setup(&mut dashboard);
    append_golden_state(
        &mut output,
        "global review form",
        100,
        30,
        &rendered(&mut dashboard, 100, 30),
    );
    writeln!(output, "action: {entry_action:?}").expect("write entry action");

    // Enabled -> Profile, then choose the first configured profile.
    dashboard.handle_key(key(KeyCode::Tab));
    let probe = choose_next(&mut dashboard);
    while dialog(&dashboard).focused() != ReviewSettingsFocus::Save {
        dashboard.handle_key(key(KeyCode::Tab));
    }
    let action = dashboard.handle_key(key(KeyCode::Enter));
    append_golden_state(
        &mut output,
        "save applies only global review values",
        100,
        30,
        &rendered(&mut dashboard, 100, 30),
    );
    writeln!(output, "discovery action: {probe:?}").expect("write discovery action");
    match action {
        DashboardAction::SaveSetup { updated, .. } => {
            let updated: serde_json::Value = serde_json::from_str(&updated).expect("saved config");
            writeln!(output, "saved global review: {}", updated["review"])
                .expect("write saved review");
        }
        other => panic!("expected global setup save, got {other:?}"),
    }

    let mut auto = dashboard_with_session(running_session());
    let _ = open_review_from_setup(&mut auto);
    let editor = dialog(&auto);
    let model_enabled = editor.form.borrow().is_enabled(ReviewSettingsFocus::Model);
    let effort_enabled = editor.form.borrow().is_enabled(ReviewSettingsFocus::Effort);
    let save_enabled = editor.can_save();
    append_golden_state(
        &mut output,
        "Auto profile leaves manual overrides disabled",
        100,
        30,
        &rendered(&mut auto, 100, 30),
    );
    writeln!(output, "profile: Auto; model enabled: {model_enabled}; effort enabled: {effort_enabled}; save enabled: {save_enabled}")
        .expect("write Auto form state");

    mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), "review-settings", &output);
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
    assert!(text.matches(ComboBox::GLYPH).count() >= 3, "{text}");
    assert!(!text.contains("Tier"), "{text}");
    assert!(
        !text.contains("Quick") && !text.contains("Extended"),
        "{text}"
    );

    while dialog(&dashboard).focused() != ReviewSettingsFocus::Profile {
        dashboard.handle_key(key(KeyCode::Tab));
    }
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert!(
        dialog(&dashboard)
            .combo
            .is_open(ReviewSettingsFocus::Profile)
    );
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
fn clearing_the_profile_cancels_discovery_without_closing_the_draft() {
    let mut dashboard = dashboard_with_session(running_session());
    open(&mut dashboard);
    dashboard.handle_key(key(KeyCode::Char(' ')));
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
