use std::collections::BTreeMap;
use std::path::PathBuf;

use crossterm::event::KeyCode;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Position;

use hel::hel_config::{HarnessKind, HarnessProfile, SshConnection, TargetTemplate};
use hel::hel_state::{HelState, HostContainerSize, STATE_VERSION, SessionResourceAllocation};
use hel::hel_targets::AdditionalMount;

use super::*;
use crate::test_support::*;

use crate::render::render;
use crate::{DashboardAction, DashboardState, Mode, nth_key};

#[test]
fn empty_workspace_prepares_codex_before_focusing_the_new_session_prompt() {
    let mut config = config();
    config
        .targets
        .insert("localhost".into(), TargetTemplate::LocalBare);
    let mut dashboard = DashboardState::new(config, HelState::default(), BTreeMap::new());
    let directory = std::env::current_dir().unwrap().join("project with spaces");
    assert_eq!(
        dashboard.begin_startup_session(directory.clone()).unwrap(),
        DashboardAction::CreateStartupSession {
            generation: None,
            initial_prompt: None,
            profile_id: "codex-1".into(),
            project_directory: directory,
            target_template_id: None,
        }
    );
    assert!(!dashboard.prompt_has_focus());
    assert!(!dashboard.modal_open());

    let mut session = running_session();
    session.state = SessionState::Provisioning;
    let mut state = HelState::default();
    state.sessions.insert(session.id.clone(), session.clone());
    dashboard.set_state(state.clone());
    dashboard.begin_session_operation(
        session.id.clone(),
        crate::SessionOperationKind::Launching,
        None,
    );
    assert!(!dashboard.prompt_has_focus());
    state.sessions.get_mut(&session.id).unwrap().state = SessionState::Running;
    dashboard.set_state(state);
    dashboard.finish_session_operation(&session.id);
    dashboard.select_active_session(&session.id);
    assert_eq!(dashboard.selected_session_id(), Some(session.id.as_str()));
    assert!(!dashboard.prompt_has_focus());
}

#[test]
fn startup_respects_the_configured_agent_and_uses_an_existing_agent_without_codex() {
    let mut config = config();
    config
        .targets
        .insert("localhost".into(), TargetTemplate::LocalBare);
    config.startup.profile = Some("claude-1".into());
    config.startup.target = Some("localhost".into());
    for explicit in [true, false] {
        if !explicit {
            config.startup.profile = None;
            config
                .profiles
                .retain(|_, profile| profile.kind != HarnessKind::Codex);
        }
        let mut dashboard =
            DashboardState::new(config.clone(), HelState::default(), BTreeMap::new());
        assert!(
            matches!(dashboard.begin_startup_session(std::env::current_dir().unwrap()).unwrap(),
            DashboardAction::CreateStartupSession { profile_id, target_template_id, .. } if profile_id == "claude-1" && target_template_id.as_deref() == Some("localhost"))
        );
    }
}

#[test]
fn startup_leaves_live_sessions_and_disabled_workspaces_alone() {
    for state in [
        SessionState::Running,
        SessionState::Provisioning,
        SessionState::Disconnected,
        SessionState::Error,
    ] {
        let mut session = running_session();
        session.state = state;
        let mut dashboard = dashboard_with_session(session);
        let selected = dashboard.selected_session_id().map(str::to_owned);
        let focus = dashboard.focus();
        assert_eq!(
            dashboard
                .begin_startup_session(std::env::current_dir().unwrap())
                .unwrap(),
            DashboardAction::None
        );
        assert_eq!(dashboard.selected_session_id(), selected.as_deref());
        assert_eq!(dashboard.focus(), focus);
    }
    let mut config = config();
    config.startup.enabled = false;
    let mut dashboard = DashboardState::new(config, HelState::default(), BTreeMap::new());
    assert_eq!(
        dashboard
            .begin_startup_session(std::env::current_dir().unwrap())
            .unwrap(),
        DashboardAction::None
    );
    assert!(!dashboard.prompt_has_focus());
}

#[test]
fn stopped_history_does_not_block_the_first_prompt() {
    let mut config = config();
    config
        .targets
        .insert("localhost".into(), TargetTemplate::LocalBare);
    let mut state = HelState::default();
    let session = stopped_session();
    state.sessions.insert(session.id.clone(), session);
    let mut dashboard = DashboardState::new(config, state, BTreeMap::new());
    assert!(matches!(
        dashboard
            .begin_startup_session(std::env::current_dir().unwrap())
            .unwrap(),
        DashboardAction::CreateStartupSession { .. }
    ));
    assert!(!dashboard.prompt_has_focus());
}

#[test]
fn startup_reports_missing_agent_profiles() {
    let mut dashboard = DashboardState::new(config(), HelState::default(), BTreeMap::new());
    let directory = std::env::current_dir().unwrap();
    dashboard.config.startup.profile = Some("missing".into());
    assert!(
        dashboard
            .begin_startup_session(directory.clone())
            .unwrap_err()
            .contains("not configured")
    );
    dashboard.config.startup.profile = None;
    dashboard.config.profiles.clear();
    assert!(
        dashboard
            .begin_startup_session(directory)
            .unwrap_err()
            .contains("F4")
    );
}

#[test]
fn new_session_wizard_returns_all_three_choices() {
    let mut dashboard = DashboardState::new(config(), HelState::default(), BTreeMap::new());
    assert_eq!(dashboard.handle_key(alt_key('w')), DashboardAction::None);
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Down)),
        DashboardAction::None
    );
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::CreateSession {
            profile_id: "codex-1".into(),
            bundle_id: "hel".into(),
            project_directory: None,
            target_template_id: "podman".into(),
            additional_mounts: vec![],
            allow_dirty_local: false,
            resource_allocation: Some(SessionResourceAllocation::Container {
                cpus: BASELINE_CPUS,
                memory_bytes: BASELINE_MEMORY_BYTES,
            }),
        }
    );
    assert!(matches!(dashboard.mode, Mode::Dashboard));
}

#[test]
fn new_session_wizard_renders_and_focuses_explicit_navigation_buttons() {
    let mut dashboard = DashboardState::new(config(), HelState::default(), BTreeMap::new());
    dashboard.handle_key(alt_key('w'));
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw wizard");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("Cancel"));
    assert!(rendered.contains("Next"));

    dashboard.handle_key(key(KeyCode::Tab));
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected new-session wizard");
    };
    assert_eq!(wizard.focus, WizardFocus::Cancel);
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert!(matches!(dashboard.mode, Mode::Dashboard));
}

#[test]
fn opening_session_wizards_prefetches_all_aws_sizes() {
    let aws_target = || TargetTemplate::AwsEc2 {
        aws_profile: None,
        region: "us-east-1".into(),
        launch_template: "hel".into(),
        launch_template_version: None,
        ssh_user: "ubuntu".into(),
        address_source: hel::hel_config::AwsAddressSource::PublicIp,
        identity_file: None,
        ssh_args: Vec::new(),
    };
    let mut config = config();
    config.targets.insert("aws-a".into(), aws_target());
    config.targets.insert("aws-b".into(), aws_target());
    let mut dashboard = DashboardState::new(config.clone(), HelState::default(), BTreeMap::new());

    assert_eq!(
        dashboard.handle_key(alt_key('w')),
        DashboardAction::ResolveAwsResourceOptions {
            target_template_ids: vec!["aws-a".into(), "aws-b".into()],
        }
    );
    let aws_b_options = vec![SessionResourceAllocation::AwsEc2 {
        instance_type: "m7i.2xlarge".into(),
        vcpus: 8,
        memory_bytes: 32 * 1024 * 1024 * 1024,
    }];
    dashboard.apply_aws_resource_options("aws-b", Ok(aws_b_options.clone()));
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected new-session wizard");
    };
    assert_eq!(wizard.aws_options["aws-b"], aws_b_options);

    let mut dashboard = DashboardState::new(
        config,
        HelState {
            version: STATE_VERSION,
            sessions: BTreeMap::from([("session-1".into(), stopped_session())]),
            mount_history: BTreeMap::new(),
            container_sizes: BTreeMap::new(),
        },
        BTreeMap::new(),
    );
    assert_eq!(
        open_resume_wizard(&mut dashboard),
        DashboardAction::ResolveAwsResourceOptions {
            target_template_ids: vec!["aws-a".into(), "aws-b".into()],
        }
    );
}

#[test]
fn persisted_import_opens_resume_wizard_for_its_id_and_keeps_defaults() {
    let mut config = config();
    config
        .targets
        .insert("z-target".into(), config.targets["podman"].clone());

    let mut imported = stopped_session();
    imported.id = "imported-session".into();
    imported.last_profile = "codex-2".into();
    imported.target_template_id = "z-target".into();

    let mut dashboard = DashboardState::new(config, HelState::default(), BTreeMap::new());
    let state = HelState {
        version: STATE_VERSION,
        sessions: BTreeMap::from([(imported.id.clone(), imported)]),
        mount_history: BTreeMap::new(),
        container_sizes: BTreeMap::new(),
    };
    dashboard.set_state(state);

    assert_eq!(
        dashboard.begin_resume_for("imported-session"),
        DashboardAction::None
    );
    let Mode::Resume(wizard) = &dashboard.mode else {
        panic!("expected the imported session to open the resume wizard");
    };
    assert_eq!(wizard.session_id, "imported-session");
    let compatible_profiles = dashboard.compatible_profiles(&wizard.session_id);
    assert_eq!(
        compatible_profiles[wizard.profile].0, "codex-2",
        "codex-2 remains the selected profile"
    );
    assert_eq!(
        nth_key(&dashboard.config.targets, wizard.target),
        "z-target",
        "z-target remains the selected target"
    );
}

#[test]
fn new_session_can_request_a_repository_when_no_bundle_exists() {
    let mut config = config();
    config.bundles.clear();
    let mut dashboard = DashboardState::new(config, HelState::default(), BTreeMap::new());
    dashboard.handle_key(alt_key('w'));
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Enter));
    for character in "example/new-repo".chars() {
        dashboard.handle_key(key(KeyCode::Char(character)));
    }
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::CreateBundle {
            source: "example/new-repo".into(),
        }
    );
}

#[test]
fn bare_ssh_new_session_selects_target_then_raw_project_without_attachments() {
    let mut config = config();
    config.targets = BTreeMap::from([(
        "machine".into(),
        TargetTemplate::SshBare {
            ssh: SshConnection {
                host: "builder.example.com".into(),
                user: None,
                identity_file: None,
                extra_args: Vec::new(),
            },
            permissions: hel::hel_config::PermissionMode::Guardian,
            workspace_prefix: ".local/share/hel/workspaces".into(),
        },
    )]);
    let mut state = HelState::default();
    state.remember_project_directory("builder.example.com", std::path::Path::new("/srv/recent"));
    state.remember_project_directory("builder.example.com", std::path::Path::new("/srv/older"));
    let mut dashboard = DashboardState::new(config, state, BTreeMap::new());

    dashboard.handle_key(alt_key('w'));
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Enter));
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected new-session wizard")
    };
    assert_eq!(wizard.project_directory, "/srv/older");
    dashboard.handle_key(key(KeyCode::Down));
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected new-session wizard")
    };
    assert_eq!(wizard.project_directory, "/srv/recent");
    while let Mode::New(wizard) = &dashboard.mode
        && !wizard.project_directory.is_empty()
    {
        dashboard.handle_key(key(KeyCode::Backspace));
    }
    for character in "/srv/project".chars() {
        dashboard.handle_key(key(KeyCode::Char(character)));
    }
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::ValidateProjectDirectory {
            target_template_id: "machine".into(),
            directory: "/srv/project".into(),
        }
    );
    dashboard.apply_project_directory_validation(
        "/srv/project",
        Err("remote project directory /srv/project does not exist or is not a directory".into()),
    );

    let mut terminal = Terminal::new(TestBackend::new(100, 28)).unwrap();
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .unwrap();
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("Error: remote project directory /srv/project does not exist"));

    dashboard.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    dashboard.handle_paste("/srv/repaired");
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("invalid project validation should keep the wizard open");
    };
    assert_eq!(wizard.project_directory, "/srv/repaired");
    assert_eq!(wizard.project_directory_error, None);
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::ValidateProjectDirectory {
            target_template_id: "machine".into(),
            directory: "/srv/repaired".into(),
        }
    );

    dashboard.apply_project_directory_validation("/srv/repaired", Ok(()));

    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .unwrap();
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("Project directory: /srv/repaired"));
    assert!(!rendered.contains("Attached directories"));

    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::CreateSession {
            profile_id: "claude-1".into(),
            bundle_id: raw_project_context_id("/srv/repaired"),
            project_directory: Some("/srv/repaired".into()),
            target_template_id: "machine".into(),
            additional_mounts: Vec::new(),
            allow_dirty_local: false,
            resource_allocation: None,
        }
    );
}

/// The review pane warns when a raw target cannot rely on a harness
/// guardian for risky actions.
#[test]
fn raw_localhost_warns_for_harnesses_without_guardian_approvals() {
    let review_text = |kind: HarnessKind| {
        let mut config = config();
        config.profiles = BTreeMap::from([(
            "profile".into(),
            HarnessProfile {
                context_window_bytes: None,
                kind,
                home: PathBuf::from("/profiles/harness"),
                environment: BTreeMap::new(),
            },
        )]);
        config.targets = BTreeMap::from([("localhost".into(), TargetTemplate::LocalBare)]);
        let mut state = HelState::default();
        state.remember_project_directory("local", std::path::Path::new("/home/me/project"));
        let mut dashboard = DashboardState::new(config, state, BTreeMap::new());
        dashboard.handle_key(alt_key('w'));
        let mut terminal = Terminal::new(TestBackend::new(180, 32)).unwrap();
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    };

    for kind in [HarnessKind::Kimi, HarnessKind::Deepseek] {
        let warning = review_text(kind);
        assert!(warning.contains("DANGER"), "{kind:?}: {warning}");
        assert!(
            warning.contains("has no guardian approval mode"),
            "{kind:?}: {warning}"
        );
    }

    for kind in [HarnessKind::Codex, HarnessKind::Claude, HarnessKind::Grok] {
        let quiet = review_text(kind);
        assert!(!quiet.contains("DANGER"), "{kind:?}: {quiet}");
    }
}

#[test]
fn raw_localhost_uses_local_project_history_and_warns_for_kimi() {
    let mut config = config();
    config.profiles = BTreeMap::from([(
        "kimi".into(),
        HarnessProfile {
            context_window_bytes: None,
            kind: HarnessKind::Kimi,
            home: PathBuf::from("/profiles/kimi"),
            environment: BTreeMap::new(),
        },
    )]);
    config.targets = BTreeMap::from([("localhost".into(), TargetTemplate::LocalBare)]);
    let mut state = HelState::default();
    state.remember_project_directory("local", std::path::Path::new("/home/me/project"));
    let mut dashboard = DashboardState::new(config, state, BTreeMap::new());

    dashboard.handle_key(alt_key('w'));
    let mut terminal = Terminal::new(TestBackend::new(140, 28)).unwrap();
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .unwrap();
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("DANGER"));

    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Enter));
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected local project directory step")
    };
    assert_eq!(wizard.project_directory, "/home/me/project");
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::ValidateProjectDirectory {
            target_template_id: "localhost".into(),
            directory: "/home/me/project".into(),
        }
    );
    dashboard.apply_project_directory_validation("/home/me/project", Ok(()));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::CreateSession {
            profile_id: "kimi".into(),
            bundle_id: raw_project_context_id("/home/me/project"),
            project_directory: Some("/home/me/project".into()),
            target_template_id: "localhost".into(),
            additional_mounts: Vec::new(),
            allow_dirty_local: false,
            resource_allocation: None,
        }
    );
}

#[test]
fn new_session_bundles_are_ordered_by_latest_session_creation() {
    let mut config = config();
    let bundle = config.bundles["hel"].clone();
    config.bundles.insert("alpha-unused".into(), bundle.clone());
    config.bundles.insert("zebra-recent".into(), bundle);

    let mut older = stopped_session();
    older.id = "older".into();
    older.created_at = "2026-08-10T12:00:00Z".into();
    let mut recent = stopped_session();
    recent.id = "recent".into();
    recent.bundle_id = "zebra-recent".into();
    recent.created_at = "2026-08-11T12:00:00Z".into();
    let state = HelState {
        version: STATE_VERSION,
        sessions: BTreeMap::from([(older.id.clone(), older), (recent.id.clone(), recent)]),
        mount_history: BTreeMap::new(),
        container_sizes: BTreeMap::new(),
    };
    assert_eq!(
        bundle_ids_by_recent_creation(&config, &state),
        vec!["zebra-recent", "hel", "alpha-unused"]
    );

    let mut dashboard = DashboardState::new(config, state, BTreeMap::new());
    dashboard.handle_key(alt_key('w'));
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Enter));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::CreateSession {
            profile_id: "codex-1".into(),
            bundle_id: "zebra-recent".into(),
            project_directory: None,
            target_template_id: "podman".into(),
            additional_mounts: vec![],
            allow_dirty_local: false,
            resource_allocation: Some(SessionResourceAllocation::Container {
                cpus: BASELINE_CPUS,
                memory_bytes: BASELINE_MEMORY_BYTES,
            }),
        }
    );
}

#[test]
fn new_session_defaults_to_the_most_recent_configured_choices() {
    let mut config = config();
    config
        .bundles
        .insert("recent-project".into(), config.bundles["hel"].clone());
    config
        .targets
        .insert("recent-target".into(), config.targets["podman"].clone());
    let mut recent = stopped_session();
    recent.last_profile = "codex-1".into();
    recent.bundle_id = "recent-project".into();
    recent.target_template_id = "recent-target".into();
    recent.created_at = "2026-08-12T12:00:00Z".into();
    let state = HelState {
        version: STATE_VERSION,
        sessions: BTreeMap::from([(recent.id.clone(), recent)]),
        mount_history: BTreeMap::new(),
        container_sizes: BTreeMap::new(),
    };
    let mut dashboard = DashboardState::new(config, state, BTreeMap::new());

    dashboard.handle_key(alt_key('w'));
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected new-session wizard");
    };
    assert_eq!(
        nth_key(&dashboard.config.profiles, wizard.profile),
        "codex-1"
    );
    assert_eq!(
        nth_bundle_key(&dashboard.config, &dashboard.state, wizard.bundle),
        "recent-project"
    );
    assert_eq!(
        nth_key(&dashboard.config.targets, wizard.target),
        "recent-target"
    );
}

/// Walk the new-session wizard as far as an open mount editor with the
/// source already typed and the destination filled in.
fn dashboard_at_mount_editor(source: &str) -> DashboardState {
    let mut dashboard = DashboardState::new(config(), HelState::default(), BTreeMap::new());
    dashboard.handle_key(alt_key('w'));
    dashboard.handle_key(key(KeyCode::Down));
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::BackTab));
    dashboard.handle_key(key(KeyCode::Enter));
    for character in source.chars() {
        dashboard.handle_key(key(KeyCode::Char(character)));
    }
    // Enter on the source fills the default destination and moves on.
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard
}

fn wizard_mounts(dashboard: &DashboardState) -> &MountWizard {
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected the new-session wizard");
    };
    &wizard.mounts
}

#[test]
fn the_read_only_checkbox_rides_the_mount_into_the_created_session() {
    let mut dashboard = dashboard_at_mount_editor("/opt/cache");

    dashboard.handle_key(key(KeyCode::Tab));
    assert_eq!(wizard_mounts(&dashboard).focus, MountFocus::ReadOnly);
    dashboard.handle_key(key(KeyCode::Char(' ')));
    assert!(wizard_mounts(&dashboard).read_only);

    // Tab past Cancel and Back to the add button, then commit.
    for _ in 0..3 {
        dashboard.handle_key(key(KeyCode::Tab));
    }
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::ValidateMountSource {
            target_template_id: "podman".into(),
            source: "/opt/cache".into(),
        }
    );
    dashboard.apply_mount_source_validation("/opt/cache", Ok(None));

    assert_eq!(
        wizard_mounts(&dashboard).mounts,
        vec![AdditionalMount {
            source: "/opt/cache".into(),
            destination: "/mnt/cache".into(),
            read_only: true,
        }]
    );
    // The next entry starts unchecked again.
    assert!(!wizard_mounts(&dashboard).read_only);
}

#[test]
fn a_source_the_host_forces_read_only_cannot_be_unchecked() {
    let mut dashboard = dashboard_at_mount_editor("/nfs/share");

    assert!(!wizard_mounts(&dashboard).read_only);
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard
        .apply_mount_source_validation("/nfs/share", Ok(Some("nfs (network filesystem)".into())));
    assert_eq!(
        wizard_mounts(&dashboard).mounts,
        vec![AdditionalMount {
            source: "/nfs/share".into(),
            destination: "/mnt/share".into(),
            read_only: true,
        }]
    );

    // Reopen the entry: the checkbox is checked, locked, and named.
    dashboard.handle_key(key(KeyCode::Enter));
    assert!(wizard_mounts(&dashboard).read_only);
    assert_eq!(
        wizard_mounts(&dashboard).forced_read_only(),
        Some("nfs (network filesystem)")
    );
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Tab));
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected mount editor");
    };
    // The locked checkbox is disabled in the shared form, so traversal skips
    // it and lands on Cancel. Space therefore belongs to the button rather
    // than attempting to toggle the forced read-only state.
    assert_eq!(wizard.form.borrow().focused(), Some(WizardControl::Cancel));
    assert_eq!(wizard_mounts(&dashboard).focus, MountFocus::Cancel);
    assert!(
        wizard_mounts(&dashboard).read_only,
        "a forced source must stay read-only"
    );

    let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw the mount editor");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("[✓] Read-only (locked)"));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Esc)),
        DashboardAction::None
    );
    assert!(matches!(dashboard.mode, Mode::Dashboard));
}

#[test]
fn new_session_mount_wizard_adds_mount_and_preserves_typed_source() {
    let mut dashboard = DashboardState::new(config(), HelState::default(), BTreeMap::new());
    dashboard.handle_key(alt_key('w'));
    dashboard.handle_key(key(KeyCode::Down));
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::BackTab));
    dashboard.handle_key(key(KeyCode::Enter));
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw resource wizard");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("Source:"));
    let lines = buffer_lines(terminal.backend().buffer());
    let source_row = lines
        .iter()
        .position(|line| line.contains("Source:"))
        .expect("source field");
    let source_x = cell_column(&lines[source_row], "Source:");
    assert_eq!(
        terminal.get_cursor_position().expect("source cursor"),
        Position {
            x: source_x + 10,
            y: source_row as u16,
        }
    );
    assert!(rendered.contains("Add directory"));
    for character in "/opt/cache".chars() {
        dashboard.handle_key(key(KeyCode::Char(character)));
    }
    dashboard.apply_mount_source_completions("/opt/ca", vec!["/opt/cache/".into()]);
    dashboard.handle_key(key(KeyCode::Enter));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::ValidateMountSource {
            target_template_id: "podman".into(),
            source: "/opt/cache".into(),
        }
    );
    dashboard.apply_mount_source_validation("/opt/cache", Ok(None));
    dashboard.handle_key(key(KeyCode::BackTab));

    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::ValidateSessionMounts {
            target_template_id: "podman".into(),
            mounts: vec![AdditionalMount {
                source: "/opt/cache".into(),
                destination: "/mnt/cache".into(),
                read_only: false,
            }],
            launch: Box::new(DashboardAction::CreateSession {
                profile_id: "codex-1".into(),
                bundle_id: "hel".into(),
                project_directory: None,
                target_template_id: "podman".into(),
                additional_mounts: vec![AdditionalMount {
                    source: "/opt/cache".into(),
                    destination: "/mnt/cache".into(),
                    read_only: false,
                }],
                allow_dirty_local: false,
                resource_allocation: Some(SessionResourceAllocation::Container {
                    cpus: BASELINE_CPUS,
                    memory_bytes: BASELINE_MEMORY_BYTES,
                }),
            }),
        }
    );
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("mount validation should keep the new-session wizard open");
    };
    assert_eq!(wizard.mounts.mounts.len(), 1);
    dashboard.finish_session_mount_preflight();
    assert!(matches!(dashboard.mode, Mode::Dashboard));
}

#[test]
fn failed_submit_preflight_reopens_the_invalid_mount() {
    let mut dashboard = dashboard_at_mount_editor("/opt/cache");
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.apply_mount_source_validation("/opt/cache", Ok(None));
    dashboard.handle_key(key(KeyCode::BackTab));
    assert!(matches!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::ValidateSessionMounts { .. }
    ));

    dashboard.apply_session_mount_preflight_failure(
        "/opt/cache",
        "source path /opt/cache does not exist or is not a directory".into(),
    );

    let Mode::New(wizard) = &dashboard.mode else {
        panic!("preflight failure should keep the new-session dialog open");
    };
    assert_eq!(wizard.step, WizardStep::Mounts);
    assert_eq!(wizard.mounts.source, "/opt/cache");
    assert_eq!(
        wizard.mounts.error.as_deref(),
        Some("source path /opt/cache does not exist or is not a directory")
    );
}

#[test]
fn directory_completion_is_bounded_and_keyboard_selectable() {
    let mut dashboard = DashboardState::new(config(), HelState::default(), BTreeMap::new());
    dashboard.handle_key(alt_key('w'));
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::BackTab));
    dashboard.handle_key(key(KeyCode::Enter));
    for character in "/opt/".chars() {
        dashboard.handle_key(key(KeyCode::Char(character)));
    }
    let candidates = (0..12)
        .map(|index| format!("/opt/directory-{index}/"))
        .collect::<Vec<_>>();
    dashboard.apply_mount_source_completions("/opt/", candidates);

    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected directory editor");
    };
    assert_eq!(wizard.mounts.completion_candidates.len(), 5);
    dashboard.handle_key(key(KeyCode::Down));
    dashboard.handle_key(key(KeyCode::Enter));
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected directory editor");
    };
    assert_eq!(wizard.mounts.source, "/opt/directory-1/");

    let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw bounded directory editor");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("Add directory"));
    assert!(rendered.contains("Cancel"));
}

#[test]
fn failed_source_validation_does_not_add_new_or_resume_mounts() {
    let mut dashboard = DashboardState::new(config(), HelState::default(), BTreeMap::new());
    dashboard.handle_key(alt_key('w'));
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::BackTab));
    dashboard.handle_key(key(KeyCode::Enter));
    for character in "/missing".chars() {
        dashboard.handle_key(key(KeyCode::Char(character)));
    }
    dashboard.handle_key(key(KeyCode::Enter));
    assert!(matches!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::ValidateMountSource { .. }
    ));
    dashboard.apply_mount_source_validation(
        "/missing",
        Err("source path /missing does not exist or is not a directory".into()),
    );
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected new-session resource dialog");
    };
    assert!(wizard.mounts.mounts.is_empty());
    assert_eq!(wizard.mounts.source, "/missing");
    assert_eq!(wizard.mounts.focus, MountFocus::Source);
    assert_eq!(
        wizard.mounts.error.as_deref(),
        Some("source path /missing does not exist or is not a directory")
    );

    let mut dashboard = dashboard_with_session(stopped_session());
    open_resume_wizard(&mut dashboard);
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::BackTab));
    dashboard.handle_key(key(KeyCode::Enter));
    for character in "/missing".chars() {
        dashboard.handle_key(key(KeyCode::Char(character)));
    }
    dashboard.handle_key(key(KeyCode::Enter));
    assert!(matches!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::ValidateMountSource { .. }
    ));
    dashboard.apply_mount_source_validation(
        "/missing",
        Err("source path /missing does not exist or is not a directory".into()),
    );
    let Mode::Resume(wizard) = &dashboard.mode else {
        panic!("expected resume resource dialog");
    };
    assert!(wizard.mounts.mounts.is_empty());
    assert_eq!(wizard.mounts.source, "/missing");
    assert_eq!(wizard.mounts.focus, MountFocus::Source);
}

#[test]
fn resume_can_convert_to_another_harness() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
    open_resume_wizard(&mut dashboard);
    dashboard.handle_key(key(KeyCode::Up));
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Enter));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::PreflightResumeRepositories {
            launch: Box::new(DashboardAction::ResumeSession {
                session_id: "session-1".into(),
                profile_id: "claude-1".into(),
                target_template_id: "podman".into(),
                additional_mounts: vec![],
                resource_allocation: Some(SessionResourceAllocation::Container {
                    cpus: BASELINE_CPUS,
                    memory_bytes: BASELINE_MEMORY_BYTES,
                }),
                discard_queue: false,
            }),
        }
    );
    assert!(matches!(dashboard.mode, Mode::Resume(_)));
    dashboard.finish_resume_repository_preflight();
    assert!(matches!(dashboard.mode, Mode::Dashboard));
}

#[test]
fn wizard_back_activation_preserves_the_draft_and_cancel_closes_it() {
    let mut dashboard = DashboardState::new(config(), HelState::default(), BTreeMap::new());
    dashboard.handle_key(alt_key('w'));
    dashboard.handle_key(key(KeyCode::Enter));

    // Target step: Tab reaches Cancel, then Back. Activating Back returns to
    // Profile while keeping the wizard open with its draft state.
    dashboard.handle_key(key(KeyCode::Tab));
    dashboard.handle_key(key(KeyCode::Tab));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("Back should keep the new-session wizard open");
    };
    assert_eq!(wizard.step, WizardStep::Profile);

    // The same explicit button path then closes the modal.
    dashboard.handle_key(key(KeyCode::Tab));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert!(matches!(dashboard.mode, Mode::Dashboard));
}

#[test]
fn resume_back_activation_preserves_the_draft_and_cancel_closes_it() {
    let mut dashboard = dashboard_with_session(stopped_session());
    open_resume_wizard(&mut dashboard);
    dashboard.handle_key(key(KeyCode::Enter));

    dashboard.handle_key(key(KeyCode::Tab));
    dashboard.handle_key(key(KeyCode::Tab));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    let Mode::Resume(wizard) = &dashboard.mode else {
        panic!("Back should keep the resume wizard open");
    };
    assert_eq!(wizard.step, WizardStep::Profile);

    dashboard.handle_key(key(KeyCode::Tab));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert!(matches!(dashboard.mode, Mode::Dashboard));
}

#[test]
fn resume_defaults_to_the_session_profile() {
    let mut dashboard = dashboard_with_session(stopped_session());
    open_resume_wizard(&mut dashboard);

    let Mode::Resume(wizard) = &dashboard.mode else {
        panic!("expected resume wizard");
    };
    let profiles = dashboard.compatible_profiles(&wizard.session_id);
    assert_eq!(profiles[wizard.profile].0, "codex-1");
}

#[test]
fn resume_defaults_to_the_previously_used_target() {
    let mut dashboard = dashboard_with_session(stopped_session());
    let target = dashboard.config.targets["podman"].clone();
    dashboard.config.targets.insert("alternate".into(), target);

    open_resume_wizard(&mut dashboard);

    let Mode::Resume(wizard) = &dashboard.mode else {
        panic!("expected resume wizard");
    };
    assert_eq!(nth_key(&dashboard.config.targets, wizard.target), "podman");
}

#[test]
fn resume_refuses_a_target_the_session_cannot_use_and_says_why() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard
        .config
        .targets
        .insert("bare".into(), TargetTemplate::LocalBare);

    open_resume_wizard(&mut dashboard);
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Up));
    assert_eq!(
        nth_key(&dashboard.config.targets, resume_wizard(&dashboard).target),
        "bare"
    );

    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );

    assert_eq!(resume_wizard(&dashboard).step, WizardStep::Target);
    let notice = dashboard.notices.current().unwrap_or_default();
    assert!(notice.contains("came from GitHub"), "{notice}");
}

#[test]
fn resume_marks_an_unusable_target_row_as_disabled() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard
        .config
        .targets
        .insert("bare".into(), TargetTemplate::LocalBare);
    open_resume_wizard(&mut dashboard);
    dashboard.handle_key(key(KeyCode::Enter));

    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .unwrap();
    let rendered = buffer_lines(terminal.backend().buffer()).join("\n");

    assert!(rendered.contains("came from GitHub"), "{rendered}");
}

fn resume_wizard(dashboard: &DashboardState) -> &ResumeWizard {
    let Mode::Resume(wizard) = &dashboard.mode else {
        panic!("expected resume wizard");
    };
    wizard
}

fn open_move_review(dashboard: &mut DashboardState) -> u64 {
    dashboard.focus_sessions();
    assert_eq!(dashboard.begin_move(), DashboardAction::None);
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None,
        "profile selection advances to the move target"
    );
    let DashboardAction::MoveSession {
        preparation_request_id: Some(request_id),
        ..
    } = dashboard.handle_key(key(KeyCode::Enter))
    else {
        panic!("entering move review should request preparation");
    };
    assert!(resume_wizard(dashboard).preparing);
    assert_eq!(resume_wizard(dashboard).step, WizardStep::Review);
    request_id
}

fn move_preparation() -> hel::hel_state::MovePreparation {
    hel::hel_state::MovePreparation {
        selection: hel::hel_state::MoveSelection {
            session_id: "session-1".into(),
            profile_id: Some("codex-1".into()),
            target_template_id: Some("podman".into()),
            additional_mounts: Some(Vec::new()),
            resource_allocation: None,
            clear_resource_allocation: false,
        },
        source_profile_id: "codex-1".into(),
        source_target_template_id: "podman".into(),
        cross_harness: false,
        active: true,
        queued_commands: Vec::new(),
        fingerprint: "fingerprint".into(),
        operation_id: "move-1".into(),
    }
}

#[test]
fn resume_dialog_attaches_an_additional_resource() {
    let mut dashboard = dashboard_with_session(stopped_session());
    open_resume_wizard(&mut dashboard);
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::BackTab));
    dashboard.handle_key(key(KeyCode::Enter));
    for character in "/opt/cache".chars() {
        dashboard.handle_key(key(KeyCode::Char(character)));
    }
    dashboard.handle_key(key(KeyCode::Enter));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::ValidateMountSource {
            target_template_id: "podman".into(),
            source: "/opt/cache".into(),
        }
    );
    dashboard.apply_mount_source_validation("/opt/cache", Ok(None));
    dashboard.handle_key(key(KeyCode::BackTab));

    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::ValidateSessionMounts {
            target_template_id: "podman".into(),
            mounts: vec![AdditionalMount {
                source: "/opt/cache".into(),
                destination: "/mnt/cache".into(),
                read_only: false,
            }],
            launch: Box::new(DashboardAction::PreflightResumeRepositories {
                launch: Box::new(DashboardAction::ResumeSession {
                    session_id: "session-1".into(),
                    profile_id: "codex-1".into(),
                    target_template_id: "podman".into(),
                    additional_mounts: vec![AdditionalMount {
                        source: "/opt/cache".into(),
                        destination: "/mnt/cache".into(),
                        read_only: false,
                    }],
                    resource_allocation: Some(SessionResourceAllocation::Container {
                        cpus: BASELINE_CPUS,
                        memory_bytes: BASELINE_MEMORY_BYTES,
                    }),
                    discard_queue: false,
                }),
            }),
        }
    );
}

#[test]
fn resume_dialog_can_remove_a_previous_resource() {
    let mut session = stopped_session();
    session.additional_mounts = vec![AdditionalMount {
        source: "/opt/old-cache".into(),
        destination: "/mnt/old-cache".into(),
        read_only: false,
    }];
    let mut dashboard = dashboard_with_session(session);
    open_resume_wizard(&mut dashboard);
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Tab));
    dashboard.handle_key(key(KeyCode::Delete));

    let Mode::Resume(wizard) = &dashboard.mode else {
        panic!("expected resume resource dialog");
    };
    assert!(wizard.mounts.mounts.is_empty());
}

#[test]
fn resume_review_edits_an_existing_attached_directory_in_place() {
    let mut session = stopped_session();
    session.additional_mounts = vec![AdditionalMount {
        source: "/opt/cache".into(),
        destination: "/mnt/cache".into(),
        read_only: false,
    }];
    let mut dashboard = dashboard_with_session(session);
    open_resume_wizard(&mut dashboard);
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Tab));
    dashboard.handle_key(key(KeyCode::Enter));

    let Mode::Resume(wizard) = &dashboard.mode else {
        panic!("expected attached-directory editor");
    };
    assert_eq!(wizard.mounts.source, "/opt/cache");
    assert_eq!(wizard.mounts.destination, "/mnt/cache");
    assert_eq!(wizard.mounts.editing_mount, Some(0));

    dashboard.handle_key(key(KeyCode::Enter));
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::ValidateMountSource {
            target_template_id: "podman".into(),
            source: "/opt/cache".into(),
        }
    );
    dashboard.apply_mount_source_validation("/opt/cache", Ok(None));
    let Mode::Resume(wizard) = &dashboard.mode else {
        panic!("expected resume review");
    };
    assert_eq!(wizard.step, WizardStep::Review);
    assert_eq!(wizard.mounts.mounts.len(), 1);
}

#[test]
fn aws_resource_destinations_default_under_the_ssh_users_home() {
    let target = TargetTemplate::AwsEc2 {
        aws_profile: None,
        region: "us-east-1".into(),
        launch_template: "hel".into(),
        launch_template_version: None,
        ssh_user: "ubuntu".into(),
        address_source: hel::hel_config::AwsAddressSource::PublicIp,
        identity_file: None,
        ssh_args: Vec::new(),
    };

    assert_eq!(
        default_resource_destination(&target, std::path::Path::new("/opt/cache"), &[]),
        std::path::PathBuf::from("/home/ubuntu/mj-resources/cache")
    );
}

#[test]
fn resume_profile_step_marks_cross_harness_profiles_as_lossy() {
    let mut dashboard = dashboard_with_session(stopped_session());
    open_resume_wizard(&mut dashboard);
    let backend = TestBackend::new(120, 24);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw dashboard");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(rendered.contains("(lossy: text-only transcript)"));
    assert!(rendered.contains("Resume · 1/3"));
    assert!(rendered.contains("Lossy: text only; tool calls + reasoning dropped."));

    dashboard.handle_key(key(KeyCode::Enter));
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw resume target step");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("Resume · 2/3"));

    dashboard.handle_key(key(KeyCode::Enter));
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw resume resource step");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("Resume · 3/3"));
}

#[test]
fn move_wizard_labels_each_step_as_move() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.focus_sessions();
    assert_eq!(dashboard.begin_move(), DashboardAction::None);

    let mut terminal = Terminal::new(TestBackend::new(120, 24)).expect("terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw move profile step");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("Move · 1/3"));
    assert!(!rendered.contains("Resume · 1/3"));

    dashboard.handle_key(key(KeyCode::Enter));
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw move target step");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("Move · 2/3"));
    assert!(!rendered.contains("Resume · 2/3"));

    let preparation_request = dashboard.handle_key(key(KeyCode::Enter));
    assert!(matches!(
        preparation_request,
        DashboardAction::MoveSession {
            preparation_request_id: Some(_),
            ..
        }
    ));
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw move review step");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("Move · 3/3"));
    assert!(!rendered.contains("Resume · 3/3"));
    assert!(rendered.contains("Checking move destination"));
}

#[test]
fn taking_move_preparation_closes_only_a_valid_confirmation_handoff() {
    let mut dashboard = dashboard_with_session(running_session());
    let request_id = open_move_review(&mut dashboard);

    // A submission that arrives before preparation is ready must leave the
    // wizard open so the user can wait or reprepare the move.
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert!(dashboard.take_move_preparation("session-1").is_none());
    assert!(matches!(dashboard.mode, Mode::Resume(_)));

    let preparation = move_preparation();
    assert!(dashboard.apply_move_preparation(request_id, preparation.clone()));

    // A stale action for another session must not consume the confirmation.
    assert!(dashboard.take_move_preparation("other-session").is_none());
    let Mode::Resume(wizard) = &dashboard.mode else {
        panic!("invalid session handoff must keep the move wizard open");
    };
    assert_eq!(wizard.preparation.as_ref(), Some(&preparation));

    // Once ready, one click emits execution directly. The controller then
    // takes the retained preparation as the lifecycle handoff.
    assert!(matches!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::MoveSession {
            preparation_request_id: None,
            ..
        }
    ));
    assert_eq!(
        dashboard.take_move_preparation("session-1"),
        Some(preparation.clone())
    );
    assert!(matches!(dashboard.mode, Mode::Dashboard));

    // State/lifecycle replies arriving after handoff must not resurrect the
    // confirmation modal or reapply the consumed preparation.
    dashboard.set_state(dashboard.state.clone());
    assert!(!dashboard.apply_move_preparation(request_id, preparation));
    assert!(matches!(dashboard.mode, Mode::Dashboard));
}

#[test]
fn failed_move_preparation_stays_visible_and_retry_requests_preparation() {
    let mut dashboard = dashboard_with_session(running_session());
    let request_id = open_move_review(&mut dashboard);
    assert!(dashboard.set_move_preparation_failed(
        "session-1",
        request_id,
        "destination is unavailable".into(),
    ));

    let Mode::Resume(wizard) = &dashboard.mode else {
        panic!("preparation failure should keep the review open");
    };
    assert!(!wizard.preparing);
    assert!(wizard.preparation.is_none());
    assert_eq!(
        wizard.preparation_error.as_deref(),
        Some("destination is unavailable")
    );

    let mut terminal = Terminal::new(TestBackend::new(120, 28)).expect("terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw failed move review");
    let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
    assert!(rendered.contains("Move preparation failed: destination is unavailable"));
    assert!(rendered.contains("Retry"));

    let retry_id = match dashboard.handle_key(key(KeyCode::Enter)) {
        DashboardAction::MoveSession {
            preparation_request_id: Some(request_id),
            ..
        } => request_id,
        action => panic!("retry should request preparation, got {action:?}"),
    };
    assert_ne!(retry_id, request_id);
    assert!(resume_wizard(&dashboard).preparing);
    assert!(resume_wizard(&dashboard).preparation_error.is_none());

    let preparation = move_preparation();
    assert!(!dashboard.apply_move_preparation(request_id, preparation.clone()));
    assert!(dashboard.apply_move_preparation(retry_id, preparation));
}

#[test]
fn stale_move_preparation_is_ignored_after_cancel_and_reopen() {
    let mut dashboard = dashboard_with_session(running_session());
    let old_request_id = open_move_review(&mut dashboard);
    let preparation = move_preparation();

    dashboard.handle_key(key(KeyCode::Esc));
    assert!(matches!(dashboard.mode, Mode::Dashboard));
    assert!(!dashboard.apply_move_preparation(old_request_id, preparation.clone()));
    assert!(!dashboard.set_move_preparation_failed(
        "session-1",
        old_request_id,
        "late failure".into(),
    ));

    let new_request_id = open_move_review(&mut dashboard);
    assert_ne!(new_request_id, old_request_id);
    assert!(!dashboard.apply_move_preparation(old_request_id, preparation));
    assert!(resume_wizard(&dashboard).preparing);
}

#[test]
fn stale_move_preparation_is_ignored_after_back_and_reentering_review() {
    let mut dashboard = dashboard_with_session(running_session());
    let old_request_id = open_move_review(&mut dashboard);
    let preparation = move_preparation();

    // Submit is disabled while loading; Tab reaches Back from the form's
    // fallback focus. Returning to the target picker invalidates the request.
    dashboard.handle_key(key(KeyCode::Tab));
    assert_eq!(resume_wizard(&dashboard).review_focus, ReviewFocus::Back);
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert_eq!(resume_wizard(&dashboard).step, WizardStep::Target);
    assert!(!dashboard.apply_move_preparation(old_request_id, preparation));

    let new_request_id = match dashboard.handle_key(key(KeyCode::Enter)) {
        DashboardAction::MoveSession {
            preparation_request_id: Some(request_id),
            ..
        } => request_id,
        action => panic!("reentering review should request preparation: {action:?}"),
    };
    assert_ne!(new_request_id, old_request_id);
    assert!(resume_wizard(&dashboard).preparing);
}

#[test]
fn queue_choice_keeps_a_ready_move_confirmation_when_only_prepared_queue_exists() {
    let mut dashboard = dashboard_with_session(running_session());
    let request_id = open_move_review(&mut dashboard);
    let mut preparation = move_preparation();
    preparation.queued_commands = vec![hel::hel_state::MaterializedQueuedPrompt {
        command_id: "queued-1".into(),
        kind: hel::hel_state::QueuedCommandKind::Prompt,
        content: Vec::new(),
        queued_at_ms: 1,
    }];
    assert!(dashboard.apply_move_preparation(request_id, preparation.clone()));

    // There is no queue in session_details; the prepared queue still owns the
    // review checkbox and changing its disposition must preserve readiness.
    assert_eq!(
        dashboard.handle_key(key(KeyCode::Char('q'))),
        DashboardAction::None
    );
    assert!(!resume_wizard(&dashboard).discard_queue);
    assert_eq!(
        resume_wizard(&dashboard).preparation.as_ref(),
        Some(&preparation)
    );

    assert!(matches!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::MoveSession {
            preparation_request_id: None,
            queue: Some(hel::hel_state::ResumeQueueDisposition::Start),
            ..
        }
    ));
}

#[test]
fn removing_a_move_attachment_invalidates_and_reprepares_the_review() {
    let mut session = running_session();
    session.additional_mounts = vec![AdditionalMount {
        source: "/opt/cache".into(),
        destination: "/mnt/cache".into(),
        read_only: false,
    }];
    let mut dashboard = dashboard_with_session(session);
    let old_request_id = open_move_review(&mut dashboard);
    assert!(dashboard.apply_move_preparation(old_request_id, move_preparation()));

    // Submit wraps to the attachment list; removing the selected directory
    // changes the move selection and immediately starts a fresh preparation.
    dashboard.handle_key(key(KeyCode::Tab));
    let new_request_id = match dashboard.handle_key(key(KeyCode::Delete)) {
        DashboardAction::MoveSession {
            preparation_request_id: Some(request_id),
            ..
        } => request_id,
        action => panic!("removing an attachment should reprepare: {action:?}"),
    };
    assert_ne!(new_request_id, old_request_id);
    let wizard = resume_wizard(&dashboard);
    assert!(wizard.mounts.mounts.is_empty());
    assert!(wizard.preparation.is_none());
    assert!(wizard.preparing);
}

#[test]
fn raw_resume_review_names_the_exact_reused_project_directory() {
    let mut session = stopped_session();
    session.target_template_id = "localhost".into();
    session.project_directory = Some("/mnt/optane/bifrost-fird".into());
    session.bundle_id = "remote-project-a66373eef659f856".into();
    let mut config = config();
    config
        .targets
        .insert("localhost".into(), TargetTemplate::LocalBare);
    let mut dashboard = DashboardState::new(
        config,
        HelState {
            version: STATE_VERSION,
            sessions: BTreeMap::from([(session.id.clone(), session)]),
            mount_history: BTreeMap::new(),
            container_sizes: BTreeMap::new(),
        },
        BTreeMap::new(),
    );
    open_resume_wizard(&mut dashboard);
    dashboard.handle_key(key(KeyCode::Enter));
    dashboard.handle_key(key(KeyCode::Enter));

    let mut terminal = Terminal::new(TestBackend::new(120, 24)).expect("terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw resume review");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(
        rendered.contains("Project directory: /mnt/optane/bifrost-fird (reused)"),
        "{rendered}"
    );
    assert!(!rendered.contains("Project: remote-project-a66373eef659f856"));
}

#[test]
fn resume_target_step_minus_halves_container_size_through_the_key_path() {
    let mut config = config();
    // Mirror the real config: an EC2 target that sorts before podman.
    config.targets.insert(
        "aws-runson".into(),
        TargetTemplate::AwsEc2 {
            aws_profile: None,
            region: "us-east-1".into(),
            launch_template: "lt-123".into(),
            launch_template_version: None,
            ssh_user: "ubuntu".into(),
            address_source: Default::default(),
            identity_file: None,
            ssh_args: Vec::new(),
        },
    );
    let mut dashboard = DashboardState::new(
        config,
        HelState {
            version: STATE_VERSION,
            sessions: BTreeMap::from([("session-1".into(), stopped_session())]),
            mount_history: BTreeMap::new(),
            container_sizes: BTreeMap::new(),
        },
        BTreeMap::new(),
    );

    dashboard.begin_resume_for("session-1");
    let Mode::Resume(wizard) = &dashboard.mode else {
        panic!("expected resume wizard, got {:?}", dashboard.mode);
    };
    assert_eq!(wizard.step, WizardStep::Profile);

    // 1/3 -> 2/3 target step; podman is the session's target.
    dashboard.handle_key(key(KeyCode::Enter));
    let Mode::Resume(wizard) = &dashboard.mode else {
        panic!("expected resume wizard on target step");
    };
    assert_eq!(wizard.step, WizardStep::Target);
    assert_eq!(
        nth_key(&dashboard.config.targets, wizard.target),
        "podman".to_string()
    );
    let gib = 1024 * 1024 * 1024;
    assert_eq!(
        wizard.resource_allocation,
        Some(SessionResourceAllocation::Container {
            cpus: 8,
            memory_bytes: 32 * gib,
        })
    );

    dashboard.handle_key(key(KeyCode::Char('-')));
    let Mode::Resume(wizard) = &dashboard.mode else {
        panic!("expected resume wizard after '-'");
    };
    assert_eq!(
        wizard.resource_allocation,
        Some(SessionResourceAllocation::Container {
            cpus: 4,
            memory_bytes: 16 * gib,
        })
    );
}

#[test]
fn new_target_step_minus_halves_container_size_when_focus_is_off_content() {
    let mut dashboard = DashboardState::new(config(), HelState::default(), BTreeMap::new());
    dashboard.handle_key(alt_key('w'));
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected new wizard, got {:?}", dashboard.mode);
    };
    assert_eq!(wizard.step, WizardStep::Profile);

    dashboard.handle_key(key(KeyCode::Enter));
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected new wizard on target step");
    };
    assert_eq!(wizard.step, WizardStep::Target);
    let gib = 1024 * 1024 * 1024;
    assert_eq!(
        wizard.resource_allocation,
        Some(SessionResourceAllocation::Container {
            cpus: 8,
            memory_bytes: 32 * gib,
        })
    );

    dashboard.handle_key(key(KeyCode::Tab));
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected new wizard after tab");
    };
    assert_ne!(wizard.focus, WizardFocus::Content);

    dashboard.handle_key(key(KeyCode::Char('-')));
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected new wizard after '-'");
    };
    assert_eq!(
        wizard.resource_allocation,
        Some(SessionResourceAllocation::Container {
            cpus: 4,
            memory_bytes: 16 * gib,
        })
    );
}

#[test]
fn new_session_defaults_to_the_latest_size_on_its_host_and_clamps_to_capacity() {
    let gib = 1024 * 1024 * 1024;
    let mut state = HelState::default();
    state.container_sizes.insert(
        "local".into(),
        HostContainerSize {
            cpus: 24,
            memory_bytes: 96 * gib,
        },
    );
    let mut dashboard = DashboardState::new(config(), state, BTreeMap::new());
    dashboard.set_deployment_capacity_targets(vec![hel::hel_targets::DeploymentCapacityTarget {
        id: "local".into(),
        host: "local".into(),
        target_ids: vec!["podman".into()],
        kind: hel::hel_targets::DeploymentCapacityKind::Host,
        local: true,
        probes: Vec::new(),
        probe_error: None,
    }]);
    dashboard.apply_deployment_capacity(
        "local",
        Ok(Some(hel::hel_targets::DeploymentCapacityUsage {
            cpu_percent: None,
            memory_used_bytes: 0,
            memory_total_bytes: 48 * gib,
            logical_cores: 12,
            disk_total_bytes: None,
        })),
        0,
    );

    dashboard.handle_key(alt_key('w'));
    dashboard.handle_key(key(KeyCode::Enter));
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected new wizard on target step");
    };
    assert_eq!(
        wizard.resource_allocation,
        Some(SessionResourceAllocation::Container {
            cpus: 12,
            memory_bytes: 48 * gib,
        })
    );
}

#[test]
fn resume_keeps_the_sessions_size_instead_of_the_hosts_latest_size() {
    let gib = 1024 * 1024 * 1024;
    let mut session = stopped_session();
    session.resource_allocation = Some(SessionResourceAllocation::Container {
        cpus: 4,
        memory_bytes: 16 * gib,
    });
    let mut state = HelState::default();
    state.sessions.insert(session.id.clone(), session);
    state.container_sizes.insert(
        "local".into(),
        HostContainerSize {
            cpus: 12,
            memory_bytes: 48 * gib,
        },
    );
    let mut dashboard = DashboardState::new(config(), state, BTreeMap::new());

    dashboard.begin_resume_for("session-1");
    dashboard.handle_key(key(KeyCode::Enter));
    let Mode::Resume(wizard) = &dashboard.mode else {
        panic!("expected resume wizard on target step");
    };
    assert_eq!(
        wizard.resource_allocation,
        Some(SessionResourceAllocation::Container {
            cpus: 4,
            memory_bytes: 16 * gib,
        })
    );
}

#[test]
fn container_size_controls_clamp_independently_halves_current_ratio_and_reset() {
    let gib = 1024 * 1024 * 1024;
    let mut allocation = Some(SessionResourceAllocation::Container {
        cpus: 8,
        memory_bytes: 32 * gib,
    });
    let limits = Some((64, 64 * gib));

    adjust_resources(&mut allocation, None, limits, KeyCode::Char('+'));
    adjust_resources(&mut allocation, None, limits, KeyCode::Char('+'));
    assert_eq!(
        allocation,
        Some(SessionResourceAllocation::Container {
            cpus: 32,
            memory_bytes: 64 * gib,
        })
    );

    adjust_resources(&mut allocation, None, limits, KeyCode::Char('-'));
    assert_eq!(
        allocation,
        Some(SessionResourceAllocation::Container {
            cpus: 16,
            memory_bytes: 32 * gib,
        })
    );
    adjust_resources(&mut allocation, None, limits, KeyCode::Char('r'));
    assert_eq!(
        allocation,
        Some(SessionResourceAllocation::Container {
            cpus: 8,
            memory_bytes: 32 * gib,
        })
    );

    adjust_resources(&mut allocation, None, limits, KeyCode::Char('c'));
    assert_eq!(
        allocation,
        Some(SessionResourceAllocation::Container {
            cpus: 16,
            memory_bytes: 32 * gib,
        })
    );
    adjust_resources(&mut allocation, None, limits, KeyCode::Char('m'));
    assert_eq!(
        allocation,
        Some(SessionResourceAllocation::Container {
            cpus: 16,
            memory_bytes: 48 * gib,
        })
    );
}

#[test]
fn container_minus_clamps_cpu_at_floor_and_keeps_halving_memory() {
    let gib = 1024 * 1024 * 1024;
    let mut allocation = Some(SessionResourceAllocation::Container {
        cpus: 2,
        memory_bytes: 32 * gib,
    });
    let limits = Some((64, 64 * gib));

    adjust_resources(&mut allocation, None, limits, KeyCode::Char('-'));
    assert_eq!(
        allocation,
        Some(SessionResourceAllocation::Container {
            cpus: 2,
            memory_bytes: 16 * gib,
        })
    );
    adjust_resources(&mut allocation, None, limits, KeyCode::Char('-'));
    assert_eq!(
        allocation,
        Some(SessionResourceAllocation::Container {
            cpus: 2,
            memory_bytes: 8 * gib,
        })
    );
}

#[test]
fn container_minus_clamps_memory_at_floor_and_keeps_halving_cpu() {
    let gib = 1024 * 1024 * 1024;
    let mut allocation = Some(SessionResourceAllocation::Container {
        cpus: 16,
        memory_bytes: 8 * gib,
    });
    let limits = Some((64, 64 * gib));

    adjust_resources(&mut allocation, None, limits, KeyCode::Char('-'));
    assert_eq!(
        allocation,
        Some(SessionResourceAllocation::Container {
            cpus: 8,
            memory_bytes: 8 * gib,
        })
    );
    adjust_resources(&mut allocation, None, limits, KeyCode::Char('-'));
    assert_eq!(
        allocation,
        Some(SessionResourceAllocation::Container {
            cpus: 4,
            memory_bytes: 8 * gib,
        })
    );
}

#[test]
fn container_minus_is_a_no_op_once_both_are_at_their_floors() {
    let gib = 1024 * 1024 * 1024;
    let mut allocation = Some(SessionResourceAllocation::Container {
        cpus: 2,
        memory_bytes: 8 * gib,
    });
    let limits = Some((64, 64 * gib));

    adjust_resources(&mut allocation, None, limits, KeyCode::Char('-'));
    assert_eq!(
        allocation,
        Some(SessionResourceAllocation::Container {
            cpus: 2,
            memory_bytes: 8 * gib,
        })
    );
}

#[test]
fn container_minus_leaves_values_already_below_floor_unchanged() {
    let gib = 1024 * 1024 * 1024;
    let mut allocation = Some(SessionResourceAllocation::Container {
        cpus: 1,
        memory_bytes: 4 * gib,
    });
    let limits = Some((64, 64 * gib));

    adjust_resources(&mut allocation, None, limits, KeyCode::Char('-'));
    assert_eq!(
        allocation,
        Some(SessionResourceAllocation::Container {
            cpus: 1,
            memory_bytes: 4 * gib,
        })
    );
}

#[test]
fn container_c_clamps_at_cpu_ceiling() {
    let gib = 1024 * 1024 * 1024;
    let mut allocation = Some(SessionResourceAllocation::Container {
        cpus: 60,
        memory_bytes: 32 * gib,
    });
    let limits = Some((64, 64 * gib));

    adjust_resources(&mut allocation, None, limits, KeyCode::Char('c'));
    assert_eq!(
        allocation,
        Some(SessionResourceAllocation::Container {
            cpus: 64,
            memory_bytes: 32 * gib,
        })
    );
}

#[test]
fn container_m_clamps_at_memory_ceiling() {
    let gib = 1024 * 1024 * 1024;
    let mut allocation = Some(SessionResourceAllocation::Container {
        cpus: 8,
        memory_bytes: 60 * gib,
    });
    let limits = Some((64, 64 * gib));

    adjust_resources(&mut allocation, None, limits, KeyCode::Char('m'));
    assert_eq!(
        allocation,
        Some(SessionResourceAllocation::Container {
            cpus: 8,
            memory_bytes: 64 * gib,
        })
    );
}

#[test]
fn ec2_size_controls_use_exact_doubling_steps() {
    let options = [8_u64, 16, 32]
        .into_iter()
        .map(|vcpus| SessionResourceAllocation::AwsEc2 {
            instance_type: format!("family.{vcpus}"),
            vcpus,
            memory_bytes: vcpus * 4 * 1024 * 1024 * 1024,
        })
        .collect::<Vec<_>>();
    let mut allocation = Some(options[0].clone());
    adjust_resources(&mut allocation, Some(&options), None, KeyCode::Char('+'));
    assert_eq!(allocation_cpus(allocation.as_ref().unwrap()), 16);
    adjust_resources(&mut allocation, Some(&options), None, KeyCode::Char('r'));
    assert_eq!(allocation_cpus(allocation.as_ref().unwrap()), 8);
}

#[test]
fn cancelling_a_wizard_invalidates_checks_before_reopening_the_same_form() {
    let mut dashboard = DashboardState::new(config(), HelState::default(), BTreeMap::new());
    dashboard.handle_key(alt_key('w'));
    let pending = dashboard.session_preflight_generation();
    dashboard.handle_key(key(KeyCode::Esc));
    dashboard.handle_key(alt_key('w'));
    assert!(matches!(dashboard.mode, Mode::New(_)));
    assert_ne!(pending, dashboard.session_preflight_generation());
}

#[test]
fn target_next_focuses_the_project_field_and_footer_keys_do_not_edit_it() {
    let mut config = config();
    config.targets.clear();
    config
        .targets
        .insert("local".into(), TargetTemplate::LocalBare);
    let mut dashboard = DashboardState::new(config, HelState::default(), BTreeMap::new());
    dashboard.handle_key(alt_key('w'));
    dashboard.handle_key(key(KeyCode::Enter));
    for _ in 0..3 {
        dashboard.handle_key(key(KeyCode::Tab));
    }
    dashboard.handle_key(key(KeyCode::Enter));
    let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .unwrap();
    assert!(dashboard.text_input_focused());
    dashboard.handle_paste("/work/project");
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("project step");
    };
    assert_eq!(wizard.project_directory, "/work/project");
    dashboard.handle_key(key(KeyCode::Tab));
    assert!(!dashboard.text_input_focused());
    dashboard.handle_key(key(KeyCode::Char('x')));
    dashboard.handle_paste("ignored");
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("project step");
    };
    assert_eq!(wizard.project_directory, "/work/project");
}

#[test]
fn resume_target_next_mouse_release_advances_to_review() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    let mut session = stopped_session();
    session.target_template_id = "localhost".into();
    session.project_directory = Some("/work/project".into());
    let mut config = config();
    config.targets.clear();
    config
        .targets
        .insert("localhost".into(), TargetTemplate::LocalBare);
    let mut state = HelState::default();
    state.sessions.insert(session.id.clone(), session);
    let mut dashboard = DashboardState::new(config, state, BTreeMap::new());
    open_resume_wizard(&mut dashboard);
    dashboard.handle_key(key(KeyCode::Enter));
    let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
    dashboard.reset_component_geometry();
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let (column, row) = (0..40)
        .find_map(|row| {
            let text = (0..140)
                .map(|column| buffer[(column, row)].symbol())
                .collect::<String>();
            text.find("  Next  ").map(|column| (column as u16 + 2, row))
        })
        .expect("Next button");
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        dashboard.handle_mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        });
        dashboard.reset_component_geometry();
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .unwrap();
    }
    let Mode::Resume(wizard) = &dashboard.mode else {
        panic!("resume wizard");
    };
    assert_eq!(wizard.step, WizardStep::Review);
}
