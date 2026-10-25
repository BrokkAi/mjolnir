use super::*;

// Managed onboarding must not overwrite existing detected profiles or require
// the user to choose a home path.
#[test]
fn managed_setup_creates_a_collision_free_profile_and_requests_login() {
    let mut dashboard = dashboard_with_session(stopped_session());
    let mut existing = dashboard.config.profiles.values().next().unwrap().clone();
    existing.kind = mj_core::config::HarnessKind::Claude;
    dashboard.config.profiles.insert("claude".into(), existing);
    dashboard.begin_settings_section("profiles", None);
    click(&mut dashboard, "Set up managed");
    dashboard.handle_key(key(KeyCode::Down));
    let action = dashboard.handle_key(key(KeyCode::Enter));

    let DashboardAction::SaveSetupAndLogin {
        updated,
        profile_id,
        ..
    } = action
    else {
        panic!("managed setup did not request save and login: {action:?}");
    };
    assert_eq!(profile_id, "claude-2");
    let saved: Value = serde_json::from_str(&updated).unwrap();
    let profile = &saved["profiles"][&profile_id];
    assert_eq!(profile["kind"], "claude");
    let home = profile["home"].as_str().unwrap();
    assert!(std::path::Path::new(home).is_absolute(), "{home}");
    assert!(
        std::path::Path::new(home).ends_with("profiles/claude-2/claude"),
        "{home}"
    );
}

// Managed onboarding must keep its login handoff while an existing inline draft
// blocks Save; correcting that field must save both edits before requesting login.
#[test]
fn managed_setup_keeps_login_intent_through_pending_field_validation() {
    let mut dashboard = setup_at(&["subagents"]);
    choose(&mut dashboard, "max_concurrent");
    replace_text(&mut dashboard, "many");
    dashboard.handle_key(key(KeyCode::Esc));
    choose(&mut dashboard, "profiles");
    click(&mut dashboard, "Set up managed");
    dashboard.handle_key(key(KeyCode::Down));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert!(!dialog.saving);
    assert_eq!(
        dialog.focused_path().unwrap(),
        ["subagents", "max_concurrent"]
    );
    assert_eq!(focused_field(dialog).input.value(), "many");
    assert!(focused_field(dialog).error.is_some());
    replace_text(&mut dashboard, "5");
    let action = dashboard.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
    let DashboardAction::SaveSetupAndLogin {
        updated,
        profile_id,
        ..
    } = action
    else {
        panic!("corrected inline field lost onboarding login: {action:?}");
    };
    let saved: Config = serde_json::from_str(&updated).unwrap();
    assert_eq!(profile_id, "claude");
    assert_eq!(saved.subagents.max_concurrent, 5);
    assert_eq!(
        saved.profiles[&profile_id].kind,
        mj_core::config::HarnessKind::Claude
    );
}

// Hard-won: 8d16bf96: unrelated settings saves failed when a configured API-key file disappeared.
#[test]
fn settings_save_api_key_profiles_without_resolving_credentials_on_the_ui_thread() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("codex");
    std::fs::create_dir(&home).unwrap();
    std::fs::write(
        home.join("config.toml"),
        "model_provider = 'deepseek'\n[model_providers.deepseek]\nname = 'DeepSeek'\nbase_url = 'https://api.deepseek.com/v1'\nenv_key = 'DEEPSEEK_API_KEY'\nwire_api = 'responses'\n",
    )
    .unwrap();
    let path = directory.path().join("config.toml");
    let secrets = directory.path().join("secrets.toml");
    std::fs::write(&secrets, "DEEPSEEK_API_KEY = 'test-secret'\n").unwrap();
    let mut stored = serde_json::to_value(Config::default()).unwrap();
    stored["profiles"] = json!({"deepseek": {
        "kind": "codex", "home": home,
        "environment": {"DEEPSEEK_API_KEY": {"from_secret": "DEEPSEEK_API_KEY"}}
    }});
    let config: Config = mj_core::config::with_secret_resolver(
        mj_core::config::SecretResolver::beside(&path),
        || serde_json::from_value(stored),
    )
    .unwrap();
    config.validate().unwrap();
    // Opening and saving must work even when credentials are unavailable to
    // the UI. The background saver resolves and validates before writing.
    std::fs::remove_file(secrets).unwrap();
    let mut dialog = SetupDialog::new(&config);
    assert!(!dialog.is_dirty());
    dialog.draft["notify"]["bell"] = json!(!config.notify.bell);
    assert!(dialog.is_dirty());
    let DashboardAction::SaveSetup { updated, .. } = dialog.save(&Default::default()) else {
        panic!("expected background save: {:?}", dialog.notice);
    };
    assert!(dialog.saving);
    assert!(!updated.contains("test-secret"));
    let updated: Value = serde_json::from_str(&updated).unwrap();
    assert_eq!(updated["notify"]["bell"], !config.notify.bell);
    assert_eq!(
        updated["profiles"]["deepseek"]["environment"]["DEEPSEEK_API_KEY"],
        json!({"from_secret": "DEEPSEEK_API_KEY"})
    );
}

#[test]
fn signing_in_on_the_profile_detail_page_is_a_settings_action() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_settings_section("profiles", Some("codex-1"));
    let Mode::Setup(dialog) = &mut dashboard.mode else {
        panic!("settings");
    };
    dialog.prepare();
    dialog.form.get_mut().focus(SetupControl::SignIn);
    let generation = dialog.generation;
    let DashboardAction::SetupLogin {
        generation: anchored,
        profile_id,
    } = dashboard.handle_key(key(KeyCode::Enter))
    else {
        panic!("Sign in starts a login for the selected profile");
    };
    assert_eq!(anchored, generation);
    assert_eq!(profile_id, "codex-1");
}

#[test]
fn the_profiles_list_shows_each_profiles_sign_in_status() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.set_profile_login_statuses(
        0,
        std::collections::BTreeMap::from([
            (
                "codex-1".to_owned(),
                mj_core::credentials::LoginStatus::SignedOut,
            ),
            (
                "claude-1".to_owned(),
                mj_core::credentials::LoginStatus::SignedIn,
            ),
        ]),
    );
    dashboard.begin_settings_section("profiles", None);
    let lines = drawn(&mut dashboard, 140, 40);
    let codex = lines
        .iter()
        .find(|line| line.contains("codex-1"))
        .unwrap_or_else(|| panic!("missing codex row: {lines:#?}"));
    assert!(codex.contains("signed out"), "{codex:?}");
    let claude = lines
        .iter()
        .find(|line| line.contains("claude-1"))
        .unwrap_or_else(|| panic!("missing claude row: {lines:#?}"));
    assert!(claude.contains("signed in"), "{claude:?}");
}

#[test]
fn an_older_sign_in_status_read_does_not_replace_a_newer_one() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.set_profile_login_statuses(
        2,
        std::collections::BTreeMap::from([(
            "codex-1".to_owned(),
            mj_core::credentials::LoginStatus::SignedOut,
        )]),
    );
    dashboard.set_profile_login_statuses(
        1,
        std::collections::BTreeMap::from([(
            "codex-1".to_owned(),
            mj_core::credentials::LoginStatus::SignedIn,
        )]),
    );
    assert_eq!(
        dashboard.profile_login_status("codex-1"),
        Some(&mj_core::credentials::LoginStatus::SignedOut)
    );
}

#[test]
fn editing_a_profile_disables_sign_in_until_the_change_is_saved() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_settings_section("profiles", Some("codex-1"));
    let Mode::Setup(dialog) = &mut dashboard.mode else {
        panic!("settings");
    };
    dialog.draft["profiles"]["codex-1"]["home"] = json!("/tmp/another-home");
    dialog.prepare();
    let sign_in = dialog
        .page_actions()
        .into_iter()
        .find(|(control, _, _)| *control == SetupControl::SignIn)
        .expect("the profiles page offers sign-in");
    assert!(!sign_in.2, "Sign in must wait for the edit to be saved");
}

#[test]
fn disabling_a_profile_clears_references_and_reports_the_cleanup() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.config.review.trigger = mj_core::config::ReviewTrigger::Turn;
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
    assert!(enabled.contains('☒'), "{enabled}");

    choose(&mut dashboard, "enabled");
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings");
    };
    assert_eq!(dialog.draft["profiles"]["codex-1"]["enabled"], false);
    assert!(dialog.draft["review"]["profile"].is_null());
    assert_eq!(dialog.draft["review"]["trigger"], "off");
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

/// On a machine with no coding agent, Detect profiles said "No agent
/// installation was found that this draft does not already have." "Draft" is
/// an internal word, and the sentence gave no next step (launch finding
/// R13-4).
// Hard-won: 60750d28: empty profile discovery did not explain that the machine had no agent.
#[test]
fn detecting_profiles_on_a_machine_without_an_agent_says_so_plainly() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    let generation = setup_dialog_mut(&mut dashboard.mode).unwrap().generation;

    dashboard.setup_discovered(
        generation,
        Ok(detection(DetectScope::Profiles, Config::default())),
    );

    let notice = setup_dialog_mut(&mut dashboard.mode)
        .unwrap()
        .notice
        .clone()
        .expect("detection reports what it did");
    assert_eq!(
        notice,
        "No coding agent installation was found on this machine. Add a profile, choose Sign in, \
         or install Codex, Claude Code, Kimi Code, Grok Build, Muse Code, OpenCode, MiMo Code, or \
         DSH, sign in to it once, and choose Detect profiles again."
    );
}

/// An agent that is installed but already has a profile is not "not found".
// Hard-won: 60750d28: discovery gave no distinct outcome when every agent profile already existed.
#[test]
fn detecting_profiles_that_all_exist_says_each_agent_already_has_one() {
    let mut dashboard = dashboard_with_session(stopped_session());
    let original = dashboard.config.clone();
    dashboard.begin_setup();
    let generation = setup_dialog_mut(&mut dashboard.mode).unwrap().generation;

    dashboard.setup_discovered(generation, Ok(detection(DetectScope::Profiles, original)));

    let notice = setup_dialog_mut(&mut dashboard.mode)
        .unwrap()
        .notice
        .clone()
        .expect("detection reports what it did");
    assert_eq!(
        notice,
        "Every coding agent found on this machine already has a profile."
    );
}

/// A profile page lists only the settings its kind reads: the guardian review
/// model and Codex profile belong to Codex alone.
// Hard-won: DSH campaign 2026-10-23: Settings showed guardian and Codex fields for DSH
#[test]
fn a_profile_page_hides_codex_only_settings_for_other_kinds() {
    let keys = |kind: &str| {
        let mut draft = json!({"profiles": {"p": {"kind": kind, "home": "/h"}}});
        schema::expand(&mut draft, &mut Vec::new());
        let path = vec!["profiles".to_owned(), "p".to_owned()];
        visible_keys(&path, draft.pointer(&pointer(&path)).unwrap())
    };
    let codex_only = ["guardian_review_model", "codex_profile"];
    assert!(
        codex_only
            .iter()
            .all(|key| keys("codex").contains(&(*key).to_owned()))
    );
    for kind in ["claude", "kimi", "grok", "muse", "opencode", "mimo", "dsh"] {
        assert!(
            !keys(kind)
                .iter()
                .any(|key| codex_only.contains(&key.as_str())),
            "{kind}"
        );
    }
}
