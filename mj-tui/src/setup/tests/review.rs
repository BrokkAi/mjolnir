use super::*;

#[test]
fn review_changes_stay_in_setup_draft_until_save_and_cancel_discards_them() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    choose(&mut dashboard, "review");
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Down));
    dashboard.handle_key(key(KeyCode::Enter));
    // Esc goes back to the first page and keeps the change in the draft, as
    // the page says it will. Launch campaign finding C-19.
    dashboard.handle_key(key(KeyCode::Esc));
    assert!(!dashboard.dialog_confirmation_open());
    assert!(!dashboard.config.review.arms_turn_review());
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings remains open after leaving review")
    };
    assert!(dialog.review_editor.is_none(), "Esc left Code Review");
    assert_eq!(dialog.draft["review"]["trigger"], "turn");
    // The Back button does the same.
    choose(&mut dashboard, "review");
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Up));
    dashboard.handle_key(key(KeyCode::Enter));
    let Mode::Setup(dialog) = &mut dashboard.mode else {
        panic!("settings")
    };
    dialog
        .review_editor
        .as_mut()
        .unwrap()
        .form
        .get_mut()
        .focus(crate::review_settings::ReviewSettingsFocus::Back);
    dashboard.handle_key(key(KeyCode::Enter));
    assert!(!dashboard.dialog_confirmation_open());
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings remains open after leaving review")
    };
    assert!(dialog.review_editor.is_none(), "Back left Code Review");
    assert_eq!(dialog.draft["review"]["trigger"], "off");
    choose(&mut dashboard, "review");
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Down));
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Esc));
    // Leaving Settings with the review change still drafted asks once.
    dashboard.handle_key(key(KeyCode::Esc));
    assert!(matches!(dashboard.mode, Mode::Confirm(_)));
    dashboard.handle_key(key(KeyCode::Right));
    dashboard.handle_key(key(KeyCode::Enter));
    assert!(!dashboard.modal_open());
    assert!(!dashboard.config.review.arms_turn_review());

    dashboard.begin_setup();
    choose(&mut dashboard, "review");
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Down));
    dashboard.handle_key(key(KeyCode::Enter));
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
    assert!(updated.review.arms_turn_review());
    dashboard.setup_saved(generation, Ok(updated));
    assert!(!dashboard.modal_open());
    assert!(dashboard.config.review.arms_turn_review());
}

#[test]
fn unsaved_account_edits_block_review_cache_and_refresh_until_setup_is_saved() {
    use crate::review_settings::{ReviewSettingsChoices, ReviewSettingsFocus};

    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.config.review.profile = Some("codex-1".into());
    dashboard.config.review.trigger = mj_core::config::ReviewTrigger::Turn;
    dashboard
        .review_settings_choices
        .insert(("codex-1".into(), None), ReviewSettingsChoices::default());
    dashboard.begin_setup();
    choose(&mut dashboard, "profiles");
    choose(&mut dashboard, "codex-1");
    choose(&mut dashboard, "home");
    dashboard.handle_paste("-changed");
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Esc));
    dashboard.handle_key(key(KeyCode::Esc));
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
    dashboard.config.review.trigger = mj_core::config::ReviewTrigger::Turn;
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
    dashboard.handle_key(key(KeyCode::Esc));
    choose(&mut dashboard, "codex-1");
    choose(&mut dashboard, "home");
    dashboard.handle_key(key(KeyCode::Enter));
    assert_eq!(
        dashboard.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
        DashboardAction::None,
        "applying an unchanged account field must retain known validation"
    );
}

/// With the reviewer on Auto, the first page says so, and visiting Code
/// Review does not change what the row says. Launch campaign finding C-5.
// Hard-won: 77fc1dad: returning from reviewer setup changed the draft summary.
#[test]
fn an_automatic_reviewer_is_summarized_the_same_before_and_after_a_visit() {
    let mut configured = config();
    configured.review.trigger = mj_core::config::ReviewTrigger::Turn;
    let mut dashboard = dashboard_with_session(stopped_session());
    *dashboard.config = configured.clone();
    dashboard.mode = Mode::Setup(SetupDialog::new(&configured));
    let row = |dashboard: &mut DashboardState| {
        drawn(dashboard, 140, 40)
            .into_iter()
            .find(|line| line.contains("Code Review"))
            .expect("a Code Review row")
    };
    let before = row(&mut dashboard);
    assert!(before.contains("Auto · picks by quota"), "{before}");
    assert!(!before.contains("no reviewer"), "{before}");
    choose(&mut dashboard, "review");
    dashboard.handle_key(key(KeyCode::Esc));
    assert_eq!(row(&mut dashboard), before);
}

// Hard-won: Settings review: nested review Back reset focus to the first root control.
#[test]
fn returning_from_code_review_restores_its_launching_button() {
    for escape in [true, false] {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.begin_setup();
        choose(&mut dashboard, "review");
        if escape {
            dashboard.handle_key(key(KeyCode::Esc));
        } else {
            click(&mut dashboard, "Back");
        }
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        assert_eq!(dialog.focused_path(), Some(vec!["review".to_owned()]));
        dashboard.handle_key(key(KeyCode::Enter));
        assert!(
            setup_dialog_mut(&mut dashboard.mode)
                .unwrap()
                .review_editor
                .is_some(),
            "Enter must reopen Code Review, not Profiles"
        );
    }
}
