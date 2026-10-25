use super::*;

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
    let editor = focused_field_mut(dialog);
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
    let mut environment = profile.environment.resolved().clone();
    profile.kind.configure_profile_home_environment(
        &profile.home,
        mj_core::config::HarnessHost::current(),
        &mut environment,
    );
    assert_eq!(environment["CODEX_HOME"], expected.to_string_lossy());
}

#[test]
fn remote_path_apply_preserves_failed_and_newer_drafts() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.config.machines.insert(
        "builder".into(),
        serde_json::from_value(json!({"kind":"ssh","host":"builder"})).unwrap(),
    );
    dashboard.config.targets.insert(
        "remote-path".into(),
        serde_json::from_value(
            json!({"kind":"ssh-bare","host":"builder","permissions":"guardian"}),
        )
        .unwrap(),
    );
    dashboard.begin_setup();
    choose(&mut dashboard, "machines");
    choose(&mut dashboard, "builder");
    choose(&mut dashboard, "workspace_prefix");
    let Mode::Setup(dialog) = &mut dashboard.mode else {
        panic!("settings");
    };
    focused_field_mut(dialog).input.set_value("~/work");
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
    assert_eq!(focused_field(dialog).input.value(), "~/work");
    assert_eq!(dialog.notice.as_deref(), Some("SSH unavailable"));
    focused_field_mut(dialog).input.set_value("~/newer");
    dashboard.setup_path_resolved(generation, &draft, &path, &value, Ok("/remote/work".into()));
    let Mode::Setup(dialog) = &mut dashboard.mode else {
        panic!("settings");
    };
    assert_eq!(focused_field(dialog).input.value(), "~/newer");
    focused_field_mut(dialog).input.set_value(&value);
    dashboard.setup_path_resolved(generation, &draft, &path, &value, Ok("/remote/work".into()));
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings");
    };
    assert_eq!(
        dialog.draft["machines"]["builder"]["workspace_prefix"],
        "/remote/work"
    );
}

/// An identity file is a file, not a directory, and it lives on the machine
/// running Mjolnir rather than on the machine it configures.
#[test]
fn setup_identity_file_completes_files_locally() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.config.machines.insert(
        "builder".into(),
        serde_json::from_value(json!({"kind":"ssh","host":"builder"})).unwrap(),
    );
    dashboard.begin_setup();
    choose(&mut dashboard, "machines");
    choose(&mut dashboard, "builder");
    choose(&mut dashboard, "identity_file");
    let Mode::Setup(dialog) = &mut dashboard.mode else {
        panic!("settings");
    };
    focused_field_mut(dialog).input.set_value("/keys/id");
    dialog.prepare();

    assert_eq!(
        dashboard.handle_key(ctrl_space()),
        DashboardAction::CompletePath {
            host: mj_core::path_completion::CompletionHost::Local,
            kind: mj_core::path_completion::CompletionKind::Any,
            prefix: "/keys/id".into(),
        }
    );
    let context = dashboard.path_input_context();
    dashboard.apply_path_completions(
        &context,
        "/keys/id",
        mj_core::path_completion::PathCompletion {
            candidates: vec!["/keys/id_ed25519".into(), "/keys/id_rsa".into()],
            insert: None,
            truncated: false,
        },
    );
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings");
    };
    let EditorInput::Path(input) = &focused_field(dialog).input else {
        panic!("path editor");
    };
    assert!(input.is_completing());

    dashboard.handle_key(key(KeyCode::Down));
    dashboard.handle_key(key(KeyCode::Enter));
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings");
    };
    assert_eq!(focused_field(dialog).input.value(), "/keys/id_rsa");
}

/// A workspace prefix belongs to the machine it is configured on, so its
/// completions come from that machine and list directories only.
#[test]
fn setup_workspace_prefix_completes_on_its_machine() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.config.machines.insert(
        "builder".into(),
        serde_json::from_value(json!({"kind":"ssh","host":"builder"})).unwrap(),
    );
    dashboard.begin_setup();
    choose(&mut dashboard, "machines");
    choose(&mut dashboard, "builder");
    choose(&mut dashboard, "workspace_prefix");
    let Mode::Setup(dialog) = &mut dashboard.mode else {
        panic!("settings");
    };
    focused_field_mut(dialog).input.set_value("/srv/w");
    dialog.prepare();

    let action = dashboard.handle_key(ctrl_space());
    let DashboardAction::CompletePath { host, kind, prefix } = action else {
        panic!("expected a completion request, got {action:?}");
    };
    assert_eq!(prefix, "/srv/w");
    assert_eq!(kind, mj_core::path_completion::CompletionKind::Directories);
    let mj_core::path_completion::CompletionHost::Machine(machine) = host else {
        panic!("a target path completes on its own machine");
    };
    assert!(matches!(*machine, mj_core::config::Machine::Ssh { .. }));
}

#[test]
fn save_resolves_pending_target_paths_and_continues_only_for_the_matching_reply() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.config.machines.insert(
        "builder".into(),
        serde_json::from_value(json!({"kind":"ssh", "host":"builder"})).unwrap(),
    );
    dashboard.begin_settings_section("machines", Some("builder"));
    choose(&mut dashboard, "workspace_prefix");
    replace_text(&mut dashboard, "~/work");
    let DashboardAction::ResolveSetupPath {
        generation,
        draft,
        path,
        value,
        ..
    } = dashboard.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
    else {
        panic!("Save resolves the pending target path");
    };
    // A stale callback cannot finish a Save after another edit.
    replace_text(&mut dashboard, "~/newer");
    dashboard.setup_path_resolved(generation, &draft, &path, &value, Ok("/remote/work".into()));
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert_eq!(focused_field(dialog).input.value(), "~/newer");
    assert!(!dialog.saving);
    let DashboardAction::ResolveSetupPath {
        generation,
        draft,
        path,
        value,
        ..
    } = dashboard.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
    else {
        panic!("resolve the new pending path");
    };
    dashboard.setup_path_resolved(
        generation.wrapping_sub(1),
        &draft,
        &path,
        &value,
        Ok("/stale/newer".into()),
    );
    assert!(matches!(
        setup_dialog_mut(&mut dashboard.mode).unwrap().pending_save,
        PendingSave::Resolving { .. }
    ));
    dashboard.setup_path_resolved(
        generation,
        &draft,
        &path,
        &value,
        Ok("/remote/newer".into()),
    );
    let DashboardAction::SaveSetup { updated, .. } = dashboard
        .take_prerequisite_check()
        .expect("Save resumes after resolution")
    else {
        panic!("background save");
    };
    let saved: Value = serde_json::from_str(&updated).unwrap();
    assert_eq!(
        saved["machines"]["builder"]["workspace_prefix"],
        "/remote/newer"
    );
}
