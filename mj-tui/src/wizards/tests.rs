use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

use crossterm::event::KeyCode;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Position;

use mj_core::config::{HarnessKind, HarnessProfile, SshConnection, TargetTemplate};
use mj_core::state::{HostContainerSize, STATE_VERSION, SessionResourceAllocation, State};

use mj_core::targets::{AdditionalMount, MountAccess};

use mj_client::quota::{ProfileQuota, QuotaWindow};

use super::*;
use crate::test_support::*;

use crate::render::render;
use crate::{DashboardAction, DashboardState, Mode, nth_key};

#[test]
fn new_session_wizard_returns_all_three_choices() {
    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    assert_eq!(ready_open_new_wizard(&mut dashboard), DashboardAction::None);
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Down)),
        DashboardAction::None
    );
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert_eq!(
        dashboard.take_prerequisite_check(),
        Some(DashboardAction::PreflightCreateSession {
            launch: Box::new(DashboardAction::CreateSession {
                mjolnir_subagents: Some(true),
                create_managed_worktree: Some(false),
                workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.into(),
                profile_id: "codex-1".into(),
                bundle_id: "hel".into(),
                project_directory: None,
                target_template_id: "podman".into(),
                additional_mounts: vec![],
                resource_allocation: Some(SessionResourceAllocation::Container {
                    cpus: BASELINE_CPUS,
                    memory_bytes: BASELINE_MEMORY_BYTES,
                }),
            }),
        })
    );
    assert!(matches!(dashboard.mode, Mode::New(_)));
}

#[test]
fn isolated_creation_review_checks_prerequisites_before_enabling_create() {
    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    ready_open_new_wizard(&mut dashboard);
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    assert!(matches!(
        dashboard.take_prerequisite_check(),
        Some(DashboardAction::PreflightCreateSession { .. })
    ));

    // Draw between inputs as the terminal does: Enter on a Create button that
    // was drawn disabled must not act until a frame shows it enabled.
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw checking review");
    assert!(
        buffer_lines(terminal.backend().buffer())
            .join("\n")
            .contains("Checking prerequisites…")
    );
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::None
    );

    let generation = dashboard.session_preflight_generation();
    dashboard.begin_remote_session_preflight(generation);
    dashboard.apply_remote_session_preflight(
        generation,
        Ok(vec![RemoteRepositoryPreview {
            repository_id: "hel".into(),
            fetch_url: "https://github.com/example/hel.git".into(),
            default_branch: "main".into(),
            push_urls: vec!["https://github.com/example/hel.git".into()],
        }]),
    );
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw ready review");
    let action = ready_key(&mut dashboard, key(KeyCode::Enter));
    assert!(matches!(action, DashboardAction::CreateSession { .. }));
    assert!(matches!(dashboard.mode, Mode::Dashboard));
}

#[test]
fn isolated_creation_runs_one_check_at_a_time_and_retries_after_failure() {
    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    ready_open_new_wizard(&mut dashboard);
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    let original_check = dashboard
        .take_prerequisite_check()
        .expect("opening the review starts its prerequisite check");
    assert!(matches!(
        original_check,
        DashboardAction::PreflightCreateSession { .. }
    ));
    assert_eq!(
        dashboard.take_prerequisite_check(),
        None,
        "a running check must not start a second resolver worker"
    );
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::None
    );

    let generation = dashboard.session_preflight_generation();
    dashboard.apply_remote_session_preflight(generation, Err("remote unavailable".into()));
    assert_eq!(
        dashboard.take_prerequisite_check(),
        None,
        "a failed check waits for Retry"
    );
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw retry review");
    assert!(
        buffer_lines(terminal.backend().buffer())
            .join("\n")
            .contains("Retry")
    );
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert_eq!(
        dashboard.take_prerequisite_check(),
        Some(original_check),
        "Retry starts the same check again"
    );
}

#[test]
fn switching_workspaces_preserves_remote_preflight_in_its_original_workspace() {
    let workspace_a = mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned();
    let workspace_b = "workspace-b".to_owned();
    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());

    ready_open_new_wizard(&mut dashboard);
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    let Some(DashboardAction::PreflightCreateSession { launch: launch_a }) =
        dashboard.take_prerequisite_check()
    else {
        panic!("network creation should start a remote preflight");
    };
    assert!(matches!(
        launch_a.as_ref(),
        DashboardAction::CreateSession { workspace_id, .. }
            if workspace_id == &workspace_a
    ));

    let generation_a = dashboard.session_preflight_generation();
    dashboard.begin_remote_session_preflight(generation_a);
    dashboard.set_active_workspace(Some(workspace_b.clone()));
    assert!(
        dashboard.modal_open(),
        "switching tabs preserves the wizard"
    );
    assert_eq!(generation_a, dashboard.session_preflight_generation());

    dashboard.apply_remote_session_preflight(
        generation_a,
        Ok(vec![RemoteRepositoryPreview {
            repository_id: "hel".into(),
            fetch_url: "https://github.com/example/hel.git".into(),
            default_branch: "main".into(),
            push_urls: vec!["https://github.com/example/hel.git".into()],
        }]),
    );
    let completed = ready_key(&mut dashboard, key(KeyCode::Enter));
    assert!(matches!(
        completed,
        DashboardAction::CreateSession { workspace_id, .. } if workspace_id == workspace_a
    ));
    assert_eq!(dashboard.active_workspace_id(), Some(workspace_b.as_str()));

    ready_open_new_wizard(&mut dashboard);
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    let Some(DashboardAction::PreflightCreateSession { launch: launch_b }) =
        dashboard.take_prerequisite_check()
    else {
        panic!("workspace B should start its own remote preflight");
    };
    assert!(matches!(
        launch_b.as_ref(),
        DashboardAction::CreateSession { workspace_id, .. }
            if workspace_id == &workspace_b
    ));
}

#[test]
fn new_session_wizard_renders_and_focuses_explicit_navigation_buttons() {
    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    ready_open_new_wizard(&mut dashboard);
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

    ready_key(&mut dashboard, key(KeyCode::Tab));
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected new-session wizard");
    };
    assert_eq!(wizard.form.borrow().focused(), Some(WizardControl::Cancel));
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
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
        address_source: mj_core::config::AwsAddressSource::PublicIp,
        identity_file: None,
        ssh_args: Vec::new(),
    };
    let mut config = config();
    config.targets.insert("aws-a".into(), aws_target());
    config.targets.insert("aws-b".into(), aws_target());
    let mut dashboard = DashboardState::new(config.clone(), State::default(), BTreeMap::new());

    assert_eq!(
        ready_open_new_wizard(&mut dashboard),
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
        State {
            subagents: Default::default(),
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

    let mut dashboard = DashboardState::new(config, State::default(), BTreeMap::new());
    let state = State {
        subagents: Default::default(),
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
    let mut dashboard = DashboardState::new(config, State::default(), BTreeMap::new());
    ready_open_new_wizard(&mut dashboard);
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    for character in "example/new-repo".chars() {
        ready_key(&mut dashboard, key(KeyCode::Char(character)));
    }
    for _ in 0..4 {
        ready_key(&mut dashboard, key(KeyCode::Tab));
    }
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::CreateBundle {
            sources: vec!["example/new-repo".into()],
        }
    );
}

fn dashboard_at_new_bundle_editor() -> DashboardState {
    let mut config = config();
    config.bundles.clear();
    let mut dashboard = DashboardState::new(config, State::default(), BTreeMap::new());
    ready_open_new_wizard(&mut dashboard);
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    assert!(matches!(
        dashboard.mode,
        Mode::New(NewWizard {
            step: WizardStep::NewBundle,
            ..
        })
    ));
    dashboard
}

fn type_source(dashboard: &mut DashboardState, source: &str) {
    for character in source.chars() {
        ready_key(dashboard, key(KeyCode::Char(character)));
    }
}

/// The bundle list holds real bundles only; the creator is a button pinned to
/// the action row's right side, the way Workspaces pins its actions.
#[test]
fn bundle_step_pins_the_new_bundle_action_beside_the_list() {
    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    dashboard.begin_new();
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    assert!(matches!(
        &dashboard.mode,
        Mode::New(wizard) if wizard.step == WizardStep::Bundle
    ));
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .unwrap();
    let lines = buffer_lines(terminal.backend().buffer());
    let list = lines.join("\n");
    assert!(list.contains("hel  1 repositories"), "{list}");
    let action_row = lines
        .iter()
        .find(|line| line.contains("New bundle…"))
        .unwrap_or_else(|| panic!("the creator is pinned in the dialog: {list}"));
    assert!(
        action_row.contains("Cancel") && action_row.contains("Next"),
        "the creator belongs to the action row, not the list: {list}"
    );
    let after_next = action_row
        .split("Next")
        .nth(1)
        .unwrap_or_default()
        .contains("New bundle…");
    assert!(
        after_next,
        "the creator sits right of the navigation buttons: {action_row}"
    );

    // Activating the pinned action opens the bundle editor.
    let Mode::New(wizard) = &mut dashboard.mode else {
        panic!("new wizard")
    };
    wizard.form.get_mut().focus(WizardControl::Add);
    ready_key(&mut dashboard, key(KeyCode::Enter));
    assert!(matches!(
        &dashboard.mode,
        Mode::New(wizard) if wizard.step == WizardStep::NewBundle
    ));
}

/// Without bundles there is nothing to select, so the creator is the only way
/// forward: the list is a hint, Next is disabled, and Enter opens the editor.
#[test]
fn bundle_step_without_bundles_routes_everything_to_the_creator() {
    let mut configuration = config();
    configuration.bundles.clear();
    let mut dashboard = DashboardState::new(configuration, State::default(), BTreeMap::new());
    dashboard.begin_new();
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    assert!(matches!(
        &dashboard.mode,
        Mode::New(wizard) if wizard.step == WizardStep::Bundle
    ));
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .unwrap();
    let text = buffer_lines(terminal.backend().buffer()).join("\n");
    assert!(text.contains("No bundles yet."), "{text}");
    assert!(text.contains("New bundle…"), "{text}");
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::None,
        "Enter falls through the empty list to the creator"
    );
    assert!(matches!(
        &dashboard.mode,
        Mode::New(wizard) if wizard.step == WizardStep::NewBundle
    ));
}

fn focus_create_bundle(dashboard: &mut DashboardState, tabs: usize) {
    for _ in 0..tabs {
        ready_key(dashboard, key(KeyCode::Tab));
    }
}

#[test]
fn new_bundle_editor_adds_multiple_repositories_and_removes_selected() {
    let mut dashboard = dashboard_at_new_bundle_editor();
    type_source(&mut dashboard, "owner/primary");
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::None
    );
    type_source(&mut dashboard, "owner/secondary");
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::None
    );
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected new-bundle editor");
    };
    assert_eq!(
        wizard.new_bundle_repositories,
        vec!["owner/primary", "owner/secondary"]
    );
    assert!(wizard.new_bundle_source.is_empty());

    // Back-tab from the source selects the list; Delete removes its selected
    // (newest) row and leaves the primary row intact.
    ready_key(&mut dashboard, key(KeyCode::BackTab));
    ready_key(&mut dashboard, key(KeyCode::Delete));
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected new-bundle editor");
    };
    assert_eq!(wizard.new_bundle_repositories, vec!["owner/primary"]);
    assert_eq!(wizard.new_bundle_selected, 0);
}

#[test]
fn new_bundle_editor_creates_from_current_source_without_add() {
    let mut dashboard = dashboard_at_new_bundle_editor();
    type_source(&mut dashboard, "owner/only");
    // Source → Add → Cancel → Back → Create. Remove is disabled while the
    // draft is empty, so the form skips it during focus navigation.
    focus_create_bundle(&mut dashboard, 4);
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::CreateBundle {
            sources: vec!["owner/only".into()]
        }
    );
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected pending new-bundle editor");
    };
    assert!(wizard.bundle_creation_in_flight);
    assert_eq!(wizard.new_bundle_source, "owner/only");
}

#[test]
fn new_bundle_editor_preserves_draft_after_failure_for_retry() {
    let mut dashboard = dashboard_at_new_bundle_editor();
    type_source(&mut dashboard, "owner/only");
    focus_create_bundle(&mut dashboard, 4);
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::CreateBundle {
            sources: vec!["owner/only".into()]
        }
    );
    dashboard.fail_bundle_creation("repository not found");
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("failure should leave the editor open");
    };
    assert!(!wizard.bundle_creation_in_flight);
    assert_eq!(wizard.new_bundle_source, "owner/only");
    assert_eq!(
        dashboard.notice().as_deref(),
        Some("Could not create bundle: repository not found")
    );
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::CreateBundle {
            sources: vec!["owner/only".into()]
        }
    );
}

#[test]
fn new_bundle_editor_submits_all_sources_once_and_advances_after_success() {
    let mut dashboard = dashboard_at_new_bundle_editor();
    type_source(&mut dashboard, "owner/primary");
    ready_key(&mut dashboard, key(KeyCode::Enter));
    type_source(&mut dashboard, "owner/secondary");
    // Add, Remove, Cancel, Back, Create.
    focus_create_bundle(&mut dashboard, 5);
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::CreateBundle {
            sources: vec!["owner/primary".into(), "owner/secondary".into()]
        }
    );
    for code in [KeyCode::Enter, KeyCode::Esc, KeyCode::Backspace] {
        assert_eq!(ready_key(&mut dashboard, key(code)), DashboardAction::None);
    }
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("creation must remain pending");
    };
    assert!(wizard.bundle_creation_in_flight);
    let created = config();
    let id = created.bundles.keys().next().unwrap().clone();
    dashboard.apply_created_bundle(created, &id);
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected session review");
    };
    assert_eq!(wizard.step, WizardStep::Review);
    assert!(!wizard.bundle_creation_in_flight);
    assert_eq!(
        nth_bundle_key(&dashboard.config, &dashboard.state, wizard.bundle),
        id
    );
}

#[test]
fn new_bundle_editor_renders_separate_input_and_help_and_accepts_mouse_add() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    let mut dashboard = dashboard_at_new_bundle_editor();
    type_source(&mut dashboard, "owner/visible");
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    dashboard.reset_component_geometry();
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .unwrap();
    let lines = buffer_lines(terminal.backend().buffer());
    let source_row = lines
        .iter()
        .position(|line| line.contains("owner/visible"))
        .unwrap();
    let help_row = lines
        .iter()
        .position(|line| line.contains("Enter adds"))
        .unwrap();
    assert_ne!(source_row, help_row);
    let rendered = lines.join("\n");
    assert!(rendered.contains("New bundle"));
    assert!(rendered.contains("Create bundle"));
    assert!(rendered.contains("GitHub source or local Git path with a network remote"));
    assert!(!rendered.contains("Create repository"));
    let (row, column) = lines
        .iter()
        .enumerate()
        .find_map(|(row, line)| {
            line.find("Add repository")
                .map(|column| (row as u16, column as u16))
        })
        .unwrap();
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
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected editor");
    };
    assert_eq!(wizard.new_bundle_repositories, ["owner/visible"]);
    assert!(wizard.new_bundle_source.is_empty());
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
            permissions: mj_core::config::PermissionMode::Guardian,
            workspace_prefix: ".local/share/hel/workspaces".into(),
        },
    )]);
    let mut state = State::default();
    state.remember_project_directory("builder.example.com", std::path::Path::new("/srv/recent"));
    state.remember_project_directory("builder.example.com", std::path::Path::new("/srv/older"));
    let mut dashboard = DashboardState::new(config, state, BTreeMap::new());

    ready_open_new_wizard(&mut dashboard);
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected new-session wizard")
    };
    assert_eq!(wizard.project_directory, "/srv/older");
    ready_key(&mut dashboard, key(KeyCode::Down));
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected new-session wizard")
    };
    assert_eq!(wizard.project_directory, "/srv/recent");
    while let Mode::New(wizard) = &dashboard.mode
        && !wizard.project_directory.is_empty()
    {
        ready_key(&mut dashboard, key(KeyCode::Backspace));
    }
    for character in "/srv/project".chars() {
        ready_key(&mut dashboard, key(KeyCode::Char(character)));
    }
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
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

    ready_key(
        &mut dashboard,
        KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
    );
    dashboard.handle_paste("/srv/repaired");
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("invalid project validation should keep the wizard open");
    };
    assert_eq!(wizard.project_directory, "/srv/repaired");
    assert_eq!(wizard.project_directory_error, None);
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::ValidateProjectDirectory {
            target_template_id: "machine".into(),
            directory: "/srv/repaired".into(),
        }
    );

    dashboard.apply_resolved_project_directory(
        &dashboard.path_input_context(),
        "/srv/repaired",
        Ok((
            PathBuf::from("/srv/repaired"),
            mj_core::state::ManagedWorktreeOptions {
                available: true,
                default_create: true,
            },
        )),
    );

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
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::CreateSession {
            mjolnir_subagents: Some(true),
            create_managed_worktree: Some(true),
            workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.into(),
            profile_id: "claude-1".into(),
            bundle_id: raw_project_context_id("/srv/repaired"),
            project_directory: Some("/srv/repaired".into()),
            target_template_id: "machine".into(),
            additional_mounts: Vec::new(),
            resource_allocation: None,
        }
    );
}

/// The profile table marks a harness that cannot guard risky actions with the
/// warning triangle and explains the marker once below the table.
#[test]
fn profile_picker_marks_harnesses_without_guardian_approvals() {
    let profile_step = |kind: HarnessKind| {
        let mut config = config();
        config.profiles = BTreeMap::from([(
            "profile".into(),
            HarnessProfile {
                enabled: true,
                context_window_bytes: None,
                guardian_review_model: None,
                kind,
                home: PathBuf::from("/profiles/harness"),
                environment: BTreeMap::new(),
            },
        )]);
        config.targets = BTreeMap::from([("localhost".into(), TargetTemplate::LocalBare)]);
        let mut state = State::default();
        state.remember_project_directory("local", std::path::Path::new("/home/me/project"));
        let mut dashboard = DashboardState::new(config, state, BTreeMap::new());
        ready_open_new_wizard(&mut dashboard);
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

    for kind in [HarnessKind::Kimi, HarnessKind::Muse] {
        let marked = profile_step(kind);
        assert!(marked.contains('⚠'), "{kind:?}: {marked}");
        assert!(
            marked.contains("No guardian approval mode"),
            "{kind:?}: {marked}"
        );
        assert!(
            marked.contains("do not run on a raw, unsandboxed target"),
            "{kind:?}: {marked}"
        );
    }

    for kind in [HarnessKind::Codex, HarnessKind::Claude, HarnessKind::Grok] {
        let quiet = profile_step(kind);
        assert!(!quiet.contains('⚠'), "{kind:?}: {quiet}");
        assert!(
            !quiet.contains("No guardian approval mode"),
            "{kind:?}: {quiet}"
        );
    }
}

/// The profile step reads as a table: the marker, profile, harness, and quota
/// columns start at the same cell on the heading and on every row.
#[test]
fn new_session_profile_step_aligns_its_columns() {
    let mut config = config();
    config.profiles.insert(
        "kimi-1".into(),
        HarnessProfile {
            enabled: true,
            context_window_bytes: None,
            guardian_review_model: None,
            kind: HarnessKind::Kimi,
            home: PathBuf::from("/profiles/kimi"),
            environment: BTreeMap::new(),
        },
    );
    let mut dashboard = DashboardState::new(config, State::default(), BTreeMap::new());
    ready_open_new_wizard(&mut dashboard);
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .unwrap();
    let lines = buffer_lines(terminal.backend().buffer());
    let row = |needle: &str| {
        lines
            .iter()
            .find(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("missing row {needle:?} in {lines:?}"))
    };

    let headings = row("PROFILE");
    let claude = row("claude-1");
    let kimi = row("kimi-1");
    let profile_column = cell_column(headings, "PROFILE");
    for (line, profile) in [(claude, "claude-1"), (kimi, "kimi-1")] {
        assert_eq!(cell_column(line, profile), profile_column);
        assert_eq!(
            cell_column(line, "refreshing"),
            cell_column(headings, "WEEKLY")
        );
    }
    assert_eq!(
        cell_column(claude, "Claude Code"),
        cell_column(headings, "HARNESS")
    );
    assert_eq!(
        cell_column(kimi, "Kimi Code"),
        cell_column(headings, "HARNESS")
    );

    // The triangle occupies the marker column, then the table gap, then the
    // profile id; only the harness without guardian approvals carries it.
    assert!(!claude.contains('⚠'));
    assert!(kimi.contains('⚠'));
    assert_eq!(
        cell_column(kimi, "⚠") + 1 + u16::try_from(COLUMN_GAP).unwrap(),
        profile_column
    );
}

/// The profile step reports quota as two percentages remaining, one per
/// window, in their own columns and with no reset countdown.
#[test]
fn new_session_profile_step_shows_weekly_and_five_hour_percentages() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.quotas = BTreeMap::from([(
        "claude-1".to_string(),
        ProfileQuota {
            profile_id: "claude-1".into(),
            harness: HarnessKind::Claude,
            windows: vec![
                QuotaWindow {
                    label: "Week".into(),
                    remaining_percent: Some(63),
                    used: None,
                    limit: None,
                    resets: Some("09:00 Aug 20".into()),
                    resets_at_epoch_seconds: Some(604_800),
                },
                QuotaWindow {
                    label: "5H".into(),
                    remaining_percent: Some(40),
                    used: None,
                    limit: None,
                    resets: Some("14:00 Aug 13".into()),
                    resets_at_epoch_seconds: Some(14_400),
                },
            ],
            extra: None,
            error: None,
            refreshed_at_epoch_seconds: 0,
        },
    )]);
    dashboard.begin_new();
    let lines = drawn(&mut dashboard, 120, 30);

    let heading_index = lines
        .iter()
        .position(|line| line.contains("PROFILE"))
        .unwrap_or_else(|| panic!("missing profile heading in {lines:?}"));
    let headings = &lines[heading_index];
    // Profiles are listed in id order, so the reported one is the first row.
    let claude = &lines[heading_index + 1];
    assert!(claude.contains("claude-1"), "{claude:?}");

    assert_eq!(
        cell_column(claude, "63%"),
        cell_column(headings, "WEEKLY"),
        "{claude:?}"
    );
    assert_eq!(
        cell_column(claude, "40%"),
        cell_column(headings, "5H"),
        "{claude:?}"
    );
    assert!(!claude.contains("09:00 Aug 20"), "{claude:?}");
    assert!(!claude.contains("resets"), "{claude:?}");
}

#[test]
fn raw_localhost_uses_local_project_history_and_warns_for_kimi() {
    let mut config = config();
    config.profiles = BTreeMap::from([(
        "kimi".into(),
        HarnessProfile {
            enabled: true,
            context_window_bytes: None,
            guardian_review_model: None,
            kind: HarnessKind::Kimi,
            home: PathBuf::from("/profiles/kimi"),
            environment: BTreeMap::new(),
        },
    )]);
    config.targets = BTreeMap::from([("localhost".into(), TargetTemplate::LocalBare)]);
    let mut state = State::default();
    state.remember_project_directory("local", std::path::Path::new("/home/me/project"));
    let mut dashboard = DashboardState::new(config, state, BTreeMap::new());

    ready_open_new_wizard(&mut dashboard);
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
    assert!(rendered.contains("No guardian approval mode"));

    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected local project directory step")
    };
    assert_eq!(wizard.project_directory, "/home/me/project");
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::ValidateProjectDirectory {
            target_template_id: "localhost".into(),
            directory: "/home/me/project".into(),
        }
    );
    dashboard.apply_resolved_project_directory(
        &dashboard.path_input_context(),
        "/home/me/project",
        Ok((
            PathBuf::from("/home/me/project"),
            mj_core::state::ManagedWorktreeOptions {
                available: true,
                default_create: true,
            },
        )),
    );
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::CreateSession {
            mjolnir_subagents: None,
            create_managed_worktree: Some(true),
            workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.into(),
            profile_id: "kimi".into(),
            bundle_id: raw_project_context_id("/home/me/project"),
            project_directory: Some("/home/me/project".into()),
            target_template_id: "localhost".into(),
            additional_mounts: Vec::new(),
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
    let state = State {
        subagents: Default::default(),
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
    ready_open_new_wizard(&mut dashboard);
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    assert_eq!(
        dashboard.take_prerequisite_check(),
        Some(DashboardAction::PreflightCreateSession {
            launch: Box::new(DashboardAction::CreateSession {
                mjolnir_subagents: Some(true),
                create_managed_worktree: Some(false),
                workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.into(),
                profile_id: "codex-1".into(),
                bundle_id: "zebra-recent".into(),
                project_directory: None,
                target_template_id: "podman".into(),
                additional_mounts: vec![],
                resource_allocation: Some(SessionResourceAllocation::Container {
                    cpus: BASELINE_CPUS,
                    memory_bytes: BASELINE_MEMORY_BYTES,
                }),
            }),
        })
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
    let state = State {
        subagents: Default::default(),
        version: STATE_VERSION,
        sessions: BTreeMap::from([(recent.id.clone(), recent)]),
        mount_history: BTreeMap::new(),
        container_sizes: BTreeMap::new(),
    };
    let mut dashboard = DashboardState::new(config, state, BTreeMap::new());

    ready_open_new_wizard(&mut dashboard);
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
    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    ready_open_new_wizard(&mut dashboard);
    ready_key(&mut dashboard, key(KeyCode::Down));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::BackTab));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    for character in source.chars() {
        ready_key(&mut dashboard, key(KeyCode::Char(character)));
    }
    // Enter on the source fills the default destination and moves on.
    ready_key(&mut dashboard, key(KeyCode::Enter));
    dashboard
}

fn new_wizard_focus(dashboard: &DashboardState) -> Option<WizardControl> {
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected new wizard")
    };
    wizard.form.borrow().focused()
}

fn wizard_mounts(dashboard: &DashboardState) -> &MountWizard {
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected the new-session wizard");
    };
    &wizard.mounts
}

/// Open the focused access combobox, move down `steps` choices, and accept.
fn choose_mount_access(dashboard: &mut DashboardState, steps: usize) {
    ready_key(dashboard, key(KeyCode::Enter));
    assert!(
        wizard_mounts(dashboard)
            .access_combo
            .is_open(WizardControl::MountAccess)
    );
    for _ in 0..steps {
        ready_key(dashboard, key(KeyCode::Down));
    }
    ready_key(dashboard, key(KeyCode::Enter));
    assert!(wizard_mounts(dashboard).access_combo.open_id().is_none());
}

#[test]
fn a_new_attachment_starts_read_only_and_the_combobox_picks_its_access() {
    let mut dashboard = dashboard_at_mount_editor("/opt/cache");

    ready_key(&mut dashboard, key(KeyCode::Tab));
    assert_eq!(
        new_wizard_focus(&dashboard),
        Some(WizardControl::MountAccess)
    );
    assert_eq!(wizard_mounts(&dashboard).access, MountAccess::Ro);
    choose_mount_access(&mut dashboard, 2);
    assert_eq!(wizard_mounts(&dashboard).access, MountAccess::Rw);

    // Tab past Cancel and Back to the add button, then commit.
    for _ in 0..3 {
        ready_key(&mut dashboard, key(KeyCode::Tab));
    }
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
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
            access: MountAccess::Rw,
        }]
    );
    // The next entry starts read-only again.
    assert_eq!(wizard_mounts(&dashboard).access, MountAccess::Ro);
}

#[test]
fn a_source_that_cannot_hold_the_overlay_skips_copy_on_write() {
    let mut dashboard = dashboard_at_mount_editor("/nfs/share");

    // Choose copy-on-write before the host reports the filesystem.
    ready_key(&mut dashboard, key(KeyCode::Tab));
    choose_mount_access(&mut dashboard, 1);
    assert_eq!(wizard_mounts(&dashboard).access, MountAccess::Cow);
    for _ in 0..3 {
        ready_key(&mut dashboard, key(KeyCode::Tab));
    }
    ready_key(&mut dashboard, key(KeyCode::Enter));
    dashboard
        .apply_mount_source_validation("/nfs/share", Ok(Some("nfs (network filesystem)".into())));
    assert_eq!(
        wizard_mounts(&dashboard).mounts,
        vec![AdditionalMount {
            source: "/nfs/share".into(),
            destination: "/mnt/share".into(),
            access: MountAccess::Ro,
        }]
    );

    // Reopen the entry: read-only and read-write remain, copy-on-write does not.
    ready_key(&mut dashboard, key(KeyCode::Enter));
    assert_eq!(wizard_mounts(&dashboard).access, MountAccess::Ro);
    assert_eq!(
        wizard_mounts(&dashboard).overlay_unavailable(),
        Some("nfs (network filesystem)")
    );
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Tab));
    assert_eq!(
        new_wizard_focus(&dashboard),
        Some(WizardControl::MountAccess)
    );
    assert_eq!(
        wizard_mounts(&dashboard).access_choices(),
        vec![MountAccess::Ro, MountAccess::Rw]
    );
    choose_mount_access(&mut dashboard, 1);
    assert_eq!(wizard_mounts(&dashboard).access, MountAccess::Rw);

    // The open list offers only the two usable modes.
    ready_key(&mut dashboard, key(KeyCode::Enter));
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
    assert!(rendered.contains("rw · writes reach the host directory"));
    assert!(rendered.contains("ro · read-only"));
    assert!(!rendered.contains("cow ·"));

    // Escape closes only the list; the changed mode is an unsaved edit, so
    // leaving the editor then asks before discarding it.
    ready_key(&mut dashboard, key(KeyCode::Esc));
    assert!(wizard_mounts(&dashboard).access_combo.open_id().is_none());
    assert!(matches!(&dashboard.mode, Mode::New(wizard) if wizard.step == WizardStep::Mounts));
    ready_key(&mut dashboard, key(KeyCode::Esc));
    assert!(dashboard.dialog_confirmation_open());
}

#[test]
fn new_session_mount_wizard_adds_mount_and_preserves_typed_source() {
    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    ready_open_new_wizard(&mut dashboard);
    ready_key(&mut dashboard, key(KeyCode::Down));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::BackTab));
    ready_key(&mut dashboard, key(KeyCode::Enter));
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
        ready_key(&mut dashboard, key(KeyCode::Char(character)));
    }
    dashboard.apply_mount_source_completions("/opt/ca", vec!["/opt/cache/".into()]);
    ready_key(&mut dashboard, key(KeyCode::Enter));
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::ValidateMountSource {
            target_template_id: "podman".into(),
            source: "/opt/cache".into(),
        }
    );
    dashboard.apply_mount_source_validation("/opt/cache", Ok(None));

    assert_eq!(
        dashboard.take_prerequisite_check(),
        Some(DashboardAction::ValidateSessionMounts {
            target_template_id: "podman".into(),
            mounts: vec![AdditionalMount {
                source: "/opt/cache".into(),
                destination: "/mnt/cache".into(),
                access: MountAccess::Ro,
            }],
            launch: Box::new(DashboardAction::CreateSession {
                mjolnir_subagents: Some(true),
                create_managed_worktree: Some(false),
                workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.into(),
                profile_id: "codex-1".into(),
                bundle_id: "hel".into(),
                project_directory: None,
                target_template_id: "podman".into(),
                additional_mounts: vec![AdditionalMount {
                    source: "/opt/cache".into(),
                    destination: "/mnt/cache".into(),
                    access: MountAccess::Ro,
                }],
                resource_allocation: Some(SessionResourceAllocation::Container {
                    cpus: BASELINE_CPUS,
                    memory_bytes: BASELINE_MEMORY_BYTES,
                }),
            }),
        })
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
    ready_key(&mut dashboard, key(KeyCode::Enter));
    dashboard.apply_mount_source_validation("/opt/cache", Ok(None));
    assert!(matches!(
        dashboard.take_prerequisite_check(),
        Some(DashboardAction::ValidateSessionMounts { .. })
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

    // The failed check is over, so correcting the directory starts a new one
    // instead of leaving the review waiting.
    dashboard.apply_mount_source_validation("/opt/cache", Ok(None));
    assert!(matches!(
        dashboard.take_prerequisite_check(),
        Some(DashboardAction::ValidateSessionMounts { .. })
    ));
}

#[test]
fn directory_completion_is_bounded_and_keyboard_selectable() {
    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    ready_open_new_wizard(&mut dashboard);
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::BackTab));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    for character in "/opt/".chars() {
        ready_key(&mut dashboard, key(KeyCode::Char(character)));
    }
    let candidates = (0..12)
        .map(|index| format!("/opt/directory-{index}/"))
        .collect::<Vec<_>>();
    dashboard.apply_mount_source_completions("/opt/", candidates);

    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected directory editor");
    };
    assert_eq!(wizard.mounts.completion_candidates.len(), 5);
    ready_key(&mut dashboard, key(KeyCode::Down));
    ready_key(&mut dashboard, key(KeyCode::Enter));
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
    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    ready_open_new_wizard(&mut dashboard);
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::BackTab));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    for character in "/missing".chars() {
        ready_key(&mut dashboard, key(KeyCode::Char(character)));
    }
    ready_key(&mut dashboard, key(KeyCode::Enter));
    assert!(matches!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
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
    assert_eq!(
        wizard.form.borrow().focused(),
        Some(WizardControl::MountSource)
    );
    assert_eq!(
        wizard.mounts.error.as_deref(),
        Some("source path /missing does not exist or is not a directory")
    );

    let mut dashboard = dashboard_with_session(stopped_session());
    open_resume_wizard(&mut dashboard);
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::BackTab));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    for character in "/missing".chars() {
        ready_key(&mut dashboard, key(KeyCode::Char(character)));
    }
    ready_key(&mut dashboard, key(KeyCode::Enter));
    assert!(matches!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
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
    assert_eq!(
        wizard.form.borrow().focused(),
        Some(WizardControl::MountSource)
    );
}

#[test]
fn resume_can_convert_to_another_harness() {
    let mut session = stopped_session();
    session.workspace_id = "workspace-history".into();
    let mut dashboard = dashboard_with_session(session);
    dashboard.set_active_workspace(Some("workspace-origin".into()));
    dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
    open_resume_wizard(&mut dashboard);
    ready_key(&mut dashboard, key(KeyCode::Up));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::PreflightResumeRepositories {
            launch: Box::new(DashboardAction::ResumeSession {
                workspace_id: "workspace-origin".into(),
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
fn resume_keeps_the_workspace_where_its_dialog_was_opened() {
    let workspace_a = mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned();
    let workspace_b = "workspace-b".to_owned();
    let mut dashboard = dashboard_with_session(stopped_session());
    open_resume_wizard(&mut dashboard);
    dashboard.set_active_workspace(Some(workspace_b));

    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    let DashboardAction::PreflightResumeRepositories { launch } =
        ready_key(&mut dashboard, key(KeyCode::Enter))
    else {
        panic!("resume should retain its captured workspace");
    };
    assert!(matches!(
        launch.as_ref(),
        DashboardAction::ResumeSession { workspace_id, .. }
            if workspace_id == &workspace_a
    ));
}

#[test]
fn wizard_back_activation_preserves_the_draft_and_cancel_closes_it() {
    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    ready_open_new_wizard(&mut dashboard);
    ready_key(&mut dashboard, key(KeyCode::Enter));

    // Target step: Tab reaches Cancel, then Back. Activating Back returns to
    // Profile while keeping the wizard open with its draft state.
    ready_key(&mut dashboard, key(KeyCode::Tab));
    ready_key(&mut dashboard, key(KeyCode::Tab));
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::None
    );
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("Back should keep the new-session wizard open");
    };
    assert_eq!(wizard.step, WizardStep::Profile);

    // The same explicit button path then closes the modal.
    ready_key(&mut dashboard, key(KeyCode::Tab));
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert!(matches!(dashboard.mode, Mode::Dashboard));
}

#[test]
fn resume_back_activation_preserves_the_draft_and_cancel_closes_it() {
    let mut dashboard = dashboard_with_session(stopped_session());
    open_resume_wizard(&mut dashboard);
    ready_key(&mut dashboard, key(KeyCode::Enter));

    ready_key(&mut dashboard, key(KeyCode::Tab));
    ready_key(&mut dashboard, key(KeyCode::Tab));
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::None
    );
    let Mode::Resume(wizard) = &dashboard.mode else {
        panic!("Back should keep the resume wizard open");
    };
    assert_eq!(wizard.step, WizardStep::Profile);

    ready_key(&mut dashboard, key(KeyCode::Tab));
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
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
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Up));
    assert_eq!(
        nth_key(&dashboard.config.targets, resume_wizard(&dashboard).target),
        "bare"
    );

    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
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
    ready_key(&mut dashboard, key(KeyCode::Enter));

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
        ready_key(dashboard, key(KeyCode::Enter)),
        DashboardAction::None,
        "profile selection advances to the move target"
    );
    let DashboardAction::MoveSession {
        preparation_request_id: Some(request_id),
        ..
    } = ready_key(dashboard, key(KeyCode::Enter))
    else {
        panic!("entering move review should request preparation");
    };
    assert!(resume_wizard(dashboard).preparing);
    assert_eq!(resume_wizard(dashboard).step, WizardStep::Review);
    request_id
}

fn move_preparation() -> mj_core::state::MovePreparation {
    mj_core::state::MovePreparation {
        in_place: false,
        source_unavailable: false,
        conversion: None,
        selection: mj_core::state::MoveSelection {
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

fn raw_conversion_preview() -> mj_core::state::RawConversionPreview {
    mj_core::state::RawConversionPreview {
        checkout: PathBuf::from("/work/repo"),
        destination: PathBuf::from("/workspace/repo"),
        branch: Some("mj/session-1".into()),
        fetch_url: "https://github.com/example/repo.git".into(),
        push_urls: vec!["https://github.com/example/repo.git".into()],
        default_branch: "main".into(),
        unpushed_commits: 2,
        staged_files: 1,
        unstaged_files: 1,
        untracked_files: 3,
        untracked_bytes: 2_621_440,
        host_checkout_retained: true,
    }
}

/// The move review is the last thing a person reads before a local checkout
/// leaves this machine, so it has to name the dirty work that travels and the
/// checkout that stays.
#[test]
fn move_review_reports_what_a_local_checkout_conversion_copies_and_leaves_behind() {
    let mut dashboard = dashboard_with_session(running_session());
    let request_id = open_move_review(&mut dashboard);
    let mut preparation = move_preparation();
    preparation.conversion = Some(Box::new(raw_conversion_preview()));
    assert!(dashboard.apply_move_preparation(request_id, preparation));

    let mut terminal = Terminal::new(TestBackend::new(200, 44)).expect("terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw move review");
    let rendered = buffer_lines(terminal.backend().buffer()).join(" ");
    let rendered = rendered.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        rendered.contains(
            "Clone https://github.com/example/repo.git (default branch main) into /workspace/repo on branch mj/session-1"
        ),
        "{rendered}"
    );
    assert!(
        rendered.contains("1 staged, 1 unstaged, and 3 untracked files (2.5 MB)"),
        "{rendered}"
    );
    assert!(
        rendered.contains("/work/repo stays on this machine and will no longer track this session"),
        "{rendered}"
    );
}

/// A profile-only move on an unchanged target keeps the container, the
/// workspace, and the untracked files, so the review must not promise a fresh
/// environment. The fresh-environment wording still has to appear when the
/// environment really is rebuilt.
#[test]
fn move_review_says_an_in_place_move_keeps_the_environment() {
    for in_place in [false, true] {
        let mut dashboard = dashboard_with_session(running_session());
        let request_id = open_move_review(&mut dashboard);
        let mut preparation = move_preparation();
        preparation.in_place = in_place;
        assert!(dashboard.apply_move_preparation(request_id, preparation));

        let mut terminal = Terminal::new(TestBackend::new(200, 44)).expect("terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw move review");
        let rendered = buffer_lines(terminal.backend().buffer()).join(" ");
        let rendered = rendered.split_whitespace().collect::<Vec<_>>().join(" ");
        if in_place {
            assert!(
                rendered.contains(
                    "Only the harness and profile are replaced; the environment and workspace are kept."
                ),
                "{rendered}"
            );
            assert!(
                !rendered.contains("fresh environment"),
                "an in-place move must not promise a fresh environment: {rendered}"
            );
        } else {
            assert!(
                rendered.contains(
                    "Active work will be interrupted; the session is restored into a fresh environment."
                ),
                "{rendered}"
            );
            assert!(
                !rendered.contains("Only the harness and profile are replaced"),
                "{rendered}"
            );
        }
    }
}

/// The resume preflight answers "this moves your checkout" before anything is
/// stopped. Nothing may launch until the person says yes, and cancelling has
/// to leave the wizard exactly where it was.
#[test]
fn a_raw_conversion_resume_launches_only_after_it_is_confirmed() {
    let mut dashboard = dashboard_with_session(stopped_session());
    open_resume_wizard(&mut dashboard);
    let before = dashboard.mode.clone();
    let launch = DashboardAction::ResumeSession {
        workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned(),
        session_id: "session-1".into(),
        profile_id: "codex-1".into(),
        target_template_id: "podman".into(),
        additional_mounts: Vec::new(),
        resource_allocation: None,
        discard_queue: false,
    };
    let receipt = mj_core::state::ResumeRepositorySourceReceipt {
        session_id: "session-1".into(),
        bundle_id: "hel".into(),
        checkpoint_sha256: "a".repeat(64),
        repositories: Vec::new(),
    };

    dashboard.show_raw_conversion_confirmation(
        launch.clone(),
        receipt.clone(),
        raw_conversion_preview(),
    );
    let mut terminal = Terminal::new(TestBackend::new(200, 44)).expect("terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw conversion confirmation");
    let rendered = buffer_lines(terminal.backend().buffer()).join(" ");
    let rendered = rendered.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        rendered.contains("2 commits not on https://github.com/example/repo.git travel"),
        "{rendered}"
    );

    // Cancel is focused first and returns to the wizard without launching.
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert_eq!(dashboard.mode, before);

    dashboard.show_raw_conversion_confirmation(
        launch.clone(),
        receipt.clone(),
        raw_conversion_preview(),
    );
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Tab)),
        DashboardAction::None
    );
    let confirmed = ready_key(&mut dashboard, key(KeyCode::Enter));
    assert_eq!(
        confirmed,
        DashboardAction::ConfirmRawConversion {
            launch: Box::new(launch),
            receipt: Box::new(receipt),
        }
    );
}

#[test]
fn resume_dialog_attaches_an_additional_resource() {
    let mut dashboard = dashboard_with_session(stopped_session());
    open_resume_wizard(&mut dashboard);
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::BackTab));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    for character in "/opt/cache".chars() {
        ready_key(&mut dashboard, key(KeyCode::Char(character)));
    }
    ready_key(&mut dashboard, key(KeyCode::Enter));
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::ValidateMountSource {
            target_template_id: "podman".into(),
            source: "/opt/cache".into(),
        }
    );
    dashboard.apply_mount_source_validation("/opt/cache", Ok(None));
    ready_key(&mut dashboard, key(KeyCode::BackTab));

    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::ValidateSessionMounts {
            target_template_id: "podman".into(),
            mounts: vec![AdditionalMount {
                source: "/opt/cache".into(),
                destination: "/mnt/cache".into(),
                access: MountAccess::Ro,
            }],
            launch: Box::new(DashboardAction::PreflightResumeRepositories {
                launch: Box::new(DashboardAction::ResumeSession {
                    workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.into(),
                    session_id: "session-1".into(),
                    profile_id: "codex-1".into(),
                    target_template_id: "podman".into(),
                    additional_mounts: vec![AdditionalMount {
                        source: "/opt/cache".into(),
                        destination: "/mnt/cache".into(),
                        access: MountAccess::Ro,
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
        access: MountAccess::Cow,
    }];
    let mut dashboard = dashboard_with_session(session);
    open_resume_wizard(&mut dashboard);
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Tab));
    ready_key(&mut dashboard, key(KeyCode::Delete));

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
        access: MountAccess::Cow,
    }];
    let mut dashboard = dashboard_with_session(session);
    open_resume_wizard(&mut dashboard);
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Tab));
    ready_key(&mut dashboard, key(KeyCode::Enter));

    let Mode::Resume(wizard) = &dashboard.mode else {
        panic!("expected attached-directory editor");
    };
    assert_eq!(wizard.mounts.source, "/opt/cache");
    assert_eq!(wizard.mounts.destination, "/mnt/cache");
    assert_eq!(wizard.mounts.editing_mount, Some(0));

    ready_key(&mut dashboard, key(KeyCode::Enter));
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
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
        address_source: mj_core::config::AwsAddressSource::PublicIp,
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

    ready_key(&mut dashboard, key(KeyCode::Enter));
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

    ready_key(&mut dashboard, key(KeyCode::Enter));
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
fn restoring_an_archive_names_the_step_and_the_archived_session() {
    let mut dashboard = dashboard_with_session(stopped_session());
    let action =
        dashboard.begin_archive_restore("wiki-1".into(), "Pomegranate work".into(), None, None);
    assert_eq!(action, crate::DashboardAction::None);
    let backend = TestBackend::new(120, 24);
    let mut terminal = Terminal::new(backend).expect("terminal");
    let draw = |dashboard: &mut DashboardState, terminal: &mut Terminal<TestBackend>| {
        terminal
            .draw(|frame| render(frame, dashboard))
            .expect("draw dashboard");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    };

    let rendered = draw(&mut dashboard, &mut terminal);
    assert!(rendered.contains("Restore · 1/3"), "{rendered}");
    assert!(rendered.contains("Pomegranate work"), "{rendered}");
    assert!(!rendered.contains("Resume · 1/3"));

    ready_key(&mut dashboard, key(KeyCode::Enter));
    let rendered = draw(&mut dashboard, &mut terminal);
    assert!(rendered.contains("Restore · 2/3"), "{rendered}");
    assert!(rendered.contains("Pomegranate work"), "{rendered}");

    ready_key(&mut dashboard, key(KeyCode::Enter));
    let rendered = draw(&mut dashboard, &mut terminal);
    assert!(rendered.contains("Restore · 3/3"), "{rendered}");
    assert!(rendered.contains("Pomegranate work"), "{rendered}");
}

#[test]
fn resume_profile_step_aligns_its_columns_and_explains_the_marker() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.config.profiles.insert(
        "kimi-1".into(),
        HarnessProfile {
            enabled: true,
            context_window_bytes: None,
            guardian_review_model: None,
            kind: HarnessKind::Kimi,
            home: PathBuf::from("/profiles/kimi"),
            environment: BTreeMap::new(),
        },
    );
    open_resume_wizard(&mut dashboard);
    let mut terminal = Terminal::new(TestBackend::new(120, 24)).expect("terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw resume profile step");
    let lines = buffer_lines(terminal.backend().buffer());
    let row = |needle: &str| {
        lines
            .iter()
            .find(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("missing row {needle:?} in {lines:?}"))
    };

    // The session runs Codex, so the Claude and Kimi rows carry the note that
    // says the cross-harness resume drops everything but text.
    let headings = row("PROFILE");
    let (profile_column, harness_column, weekly_column) = (
        cell_column(headings, "PROFILE"),
        cell_column(headings, "HARNESS"),
        cell_column(headings, "WEEKLY"),
    );
    let mut note_column = None;
    for (id, harness, lossy) in [
        ("claude-1", "Claude Code", true),
        ("codex-1", "Codex", false),
        ("kimi-1", "Kimi Code", true),
    ] {
        let line = row(id);
        assert_eq!(cell_column(line, id), profile_column, "{id}: {line}");
        assert_eq!(cell_column(line, harness), harness_column, "{id}: {line}");
        assert_eq!(
            cell_column(line, "refreshing"),
            weekly_column,
            "{id}: {line}"
        );
        if lossy {
            let column = cell_column(line, "(lossy: text-only transcript)");
            assert!(column > weekly_column, "{id}: {line}");
            assert_eq!(column, *note_column.get_or_insert(column), "{id}: {line}");
        } else {
            assert!(!line.contains("(lossy"), "{id}: {line}");
        }
    }

    // Only the Kimi row lacks guardian approvals, and the row below the table
    // explains the marker it carries.
    assert!(row("kimi-1").contains('⚠'));
    assert!(!row("claude-1").contains('⚠'));
    assert!(!row("codex-1").contains('⚠'));
    assert!(
        lines
            .iter()
            .any(|line| line.contains("No guardian approval mode; do not run on a raw")),
        "{lines:?}"
    );
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

    ready_key(&mut dashboard, key(KeyCode::Enter));
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

    let preparation_request = ready_key(&mut dashboard, key(KeyCode::Enter));
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
        ready_key(&mut dashboard, key(KeyCode::Enter)),
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
        ready_key(&mut dashboard, key(KeyCode::Enter)),
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

    let retry_id = match ready_key(&mut dashboard, key(KeyCode::Enter)) {
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

    ready_key(&mut dashboard, key(KeyCode::Esc));
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
    ready_key(&mut dashboard, key(KeyCode::Tab));
    assert_eq!(
        resume_wizard(&dashboard).form.borrow().focused(),
        Some(WizardControl::Back)
    );
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert_eq!(resume_wizard(&dashboard).step, WizardStep::Target);
    assert!(!dashboard.apply_move_preparation(old_request_id, preparation));

    let new_request_id = match ready_key(&mut dashboard, key(KeyCode::Enter)) {
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
    preparation.queued_commands = vec![mj_core::state::MaterializedQueuedPrompt {
        accepted_ordinal: None,
        command_id: "queued-1".into(),
        kind: mj_core::state::QueuedCommandKind::Prompt,
        content: Vec::new(),
        queued_at_ms: 1,
    }];
    assert!(dashboard.apply_move_preparation(request_id, preparation.clone()));

    // There is no queue in session_details; the prepared queue still owns the
    // review checkbox and changing its disposition must preserve readiness.
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Char('q'))),
        DashboardAction::None
    );
    assert!(!resume_wizard(&dashboard).discard_queue);
    assert_eq!(
        resume_wizard(&dashboard).preparation.as_ref(),
        Some(&preparation)
    );

    assert!(matches!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::MoveSession {
            preparation_request_id: None,
            queue: Some(mj_core::state::ResumeQueueDisposition::Start),
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
        access: MountAccess::Cow,
    }];
    let mut dashboard = dashboard_with_session(session);
    let old_request_id = open_move_review(&mut dashboard);
    assert!(dashboard.apply_move_preparation(old_request_id, move_preparation()));

    // Submit wraps to the attachment list; removing the selected directory
    // changes the move selection and immediately starts a fresh preparation.
    ready_key(&mut dashboard, key(KeyCode::Tab));
    let new_request_id = match ready_key(&mut dashboard, key(KeyCode::Delete)) {
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
        State {
            subagents: Default::default(),
            version: STATE_VERSION,
            sessions: BTreeMap::from([(session.id.clone(), session)]),
            mount_history: BTreeMap::new(),
            container_sizes: BTreeMap::new(),
        },
        BTreeMap::new(),
    );
    open_resume_wizard(&mut dashboard);
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));

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
        State {
            subagents: Default::default(),
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
    ready_key(&mut dashboard, key(KeyCode::Enter));
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

    ready_key(&mut dashboard, key(KeyCode::Char('-')));
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
    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    ready_open_new_wizard(&mut dashboard);
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected new wizard, got {:?}", dashboard.mode);
    };
    assert_eq!(wizard.step, WizardStep::Profile);

    ready_key(&mut dashboard, key(KeyCode::Enter));
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

    ready_key(&mut dashboard, key(KeyCode::Tab));
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected new wizard after tab");
    };
    assert_ne!(
        wizard.form.borrow().focused(),
        Some(WizardControl::TargetList)
    );

    ready_key(&mut dashboard, key(KeyCode::Char('-')));
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
    let mut state = State::default();
    state.container_sizes.insert(
        "local".into(),
        HostContainerSize {
            cpus: 24,
            memory_bytes: 96 * gib,
        },
    );
    let mut dashboard = DashboardState::new(config(), state, BTreeMap::new());
    dashboard.set_deployment_capacity_targets(vec![mj_core::targets::DeploymentCapacityTarget {
        id: "local".into(),
        host: "local".into(),
        target_ids: vec!["podman".into()],
        kind: mj_core::targets::DeploymentCapacityKind::Host,
        local: true,
        probes: Vec::new(),
        probe_error: None,
    }]);
    dashboard.apply_deployment_capacity(
        "local",
        Ok(Some(mj_core::targets::DeploymentCapacityUsage {
            cpu_percent: None,
            memory_used_bytes: 0,
            memory_total_bytes: 48 * gib,
            logical_cores: 12,
            disk_total_bytes: None,
        })),
        0,
    );

    ready_open_new_wizard(&mut dashboard);
    ready_key(&mut dashboard, key(KeyCode::Enter));
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
    let mut state = State::default();
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
    ready_key(&mut dashboard, key(KeyCode::Enter));
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
    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    ready_open_new_wizard(&mut dashboard);
    let pending = dashboard.session_preflight_generation();
    ready_key(&mut dashboard, key(KeyCode::Esc));
    ready_open_new_wizard(&mut dashboard);
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
    let mut dashboard = DashboardState::new(config, State::default(), BTreeMap::new());
    ready_open_new_wizard(&mut dashboard);
    ready_key(&mut dashboard, key(KeyCode::Enter));
    for _ in 0..3 {
        ready_key(&mut dashboard, key(KeyCode::Tab));
    }
    ready_key(&mut dashboard, key(KeyCode::Enter));
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
    ready_key(&mut dashboard, key(KeyCode::Tab));
    assert!(!dashboard.text_input_focused());
    ready_key(&mut dashboard, key(KeyCode::Char('x')));
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
    let mut state = State::default();
    state.sessions.insert(session.id.clone(), session);
    let mut dashboard = DashboardState::new(config, state, BTreeMap::new());
    open_resume_wizard(&mut dashboard);
    ready_key(&mut dashboard, key(KeyCode::Enter));
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

#[test]
fn home_mount_apply_uses_resolved_source_and_ignores_changed_destination() {
    let mut dashboard = dashboard_at_mount_editor("~/cache");
    assert!(validate_mount_entry(wizard_mounts(&dashboard)).is_none());
    let context = dashboard.path_input_context();
    let Mode::New(wizard) = &mut dashboard.mode else {
        panic!("new wizard");
    };
    wizard.mounts.destination.set_value("/mnt/newer");
    dashboard.apply_resolved_mount_source(&context, "~/cache", Ok(("/remote/cache".into(), None)));
    assert!(wizard_mounts(&dashboard).mounts.is_empty());
    let context = dashboard.path_input_context();
    dashboard.apply_resolved_mount_source(&context, "~/cache", Ok(("/remote/cache".into(), None)));
    assert_eq!(
        wizard_mounts(&dashboard).mounts[0].source,
        PathBuf::from("/remote/cache")
    );
    assert_eq!(
        wizard_mounts(&dashboard).mounts[0].destination,
        PathBuf::from("/mnt/newer")
    );
}

#[test]
fn container_destination_rejects_home_shorthand() {
    let mut dashboard = dashboard_at_mount_editor("~/cache");
    let Mode::New(wizard) = &mut dashboard.mode else {
        panic!("new wizard");
    };
    wizard.mounts.destination.set_value("~/cache");
    assert!(
        validate_mount_entry(&wizard.mounts)
            .unwrap()
            .contains("container path")
    );
}

#[test]
fn revisiting_or_reselecting_a_target_preserves_edited_resources() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.begin_new();
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Char('-')));
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("new wizard")
    };
    let edited = wizard.resource_allocation.clone();
    ready_key(&mut dashboard, key(KeyCode::Home));
    let Mode::New(wizard) = &mut dashboard.mode else {
        panic!("new wizard")
    };
    assert_eq!(wizard.resource_allocation, edited);
    wizard.form.get_mut().focus(WizardControl::Back);
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("new wizard")
    };
    assert_eq!(wizard.step, WizardStep::Target);
    assert_eq!(wizard.resource_allocation, edited);
}

#[test]
fn raw_review_waits_for_worktree_inspection_and_preserves_explicit_selection() {
    let mut configuration = config();
    configuration.targets.clear();
    configuration
        .targets
        .insert("local".into(), TargetTemplate::LocalBare);
    let mut dashboard = DashboardState::new(configuration, State::default(), BTreeMap::new());
    dashboard.begin_new();
    let Mode::New(wizard) = &mut dashboard.mode else {
        panic!("new wizard")
    };
    wizard.step = WizardStep::Review;
    wizard.project_directory = "/work/main".into();
    assert!(matches!(
        dashboard.take_prerequisite_check(),
        Some(DashboardAction::ValidateProjectDirectory { .. })
    ));
    let context = dashboard.path_input_context();
    dashboard.apply_resolved_project_directory(
        &context,
        "/work/main",
        Ok((
            PathBuf::from("/work/main"),
            mj_core::state::ManagedWorktreeOptions {
                available: true,
                default_create: true,
            },
        )),
    );
    let Mode::New(wizard) = &mut dashboard.mode else {
        panic!("new wizard")
    };
    assert!(wizard.create_managed_worktree);
    wizard
        .form
        .get_mut()
        .focus(WizardControl::CreateManagedWorktree);
    ready_key(&mut dashboard, key(KeyCode::Char(' ')));
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("new wizard")
    };
    assert!(!wizard.create_managed_worktree);
    // Revalidating an unchanged directory keeps the explicit override.
    dashboard.apply_resolved_project_directory(
        &context,
        "/work/main",
        Ok((
            PathBuf::from("/work/main"),
            mj_core::state::ManagedWorktreeOptions {
                available: true,
                default_create: true,
            },
        )),
    );
    let Mode::New(wizard) = &mut dashboard.mode else {
        panic!("new wizard")
    };
    assert!(!wizard.create_managed_worktree);
    wizard.form.get_mut().focus(WizardControl::Submit);
    assert!(
        matches!(ready_key(&mut dashboard, key(KeyCode::Enter)), DashboardAction::CreateSession {
        create_managed_worktree: Some(false), project_directory: Some(directory), ..
    } if directory == std::path::Path::new("/work/main"))
    );
}

/// Isolated targets provide the workspace themselves, so the review must not
/// show a disabled worktree checkbox at all; a bare project keeps the choice.
#[test]
fn review_hides_the_worktree_choice_for_isolated_targets() {
    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    dashboard.begin_new();
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    assert!(matches!(
        &dashboard.mode,
        Mode::New(wizard) if wizard.step == WizardStep::Review
    ));
    let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .unwrap();
    let isolated = buffer_lines(terminal.backend().buffer()).join("\n");
    assert!(!isolated.contains("Create managed worktree"), "{isolated}");
    assert!(
        !isolated.contains("isolated workspace"),
        "the checkbox and its explanation are gone together: {isolated}"
    );

    let mut configuration = config();
    configuration.targets.clear();
    configuration
        .targets
        .insert("local".into(), TargetTemplate::LocalBare);
    let mut dashboard = DashboardState::new(configuration, State::default(), BTreeMap::new());
    dashboard.begin_new();
    let Mode::New(wizard) = &mut dashboard.mode else {
        panic!("new wizard")
    };
    wizard.step = WizardStep::Review;
    wizard.project_directory = "/work/main".into();
    dashboard.apply_resolved_project_directory(
        &dashboard.path_input_context(),
        "/work/main",
        Ok((
            PathBuf::from("/work/main"),
            mj_core::state::ManagedWorktreeOptions {
                available: true,
                default_create: true,
            },
        )),
    );
    let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .unwrap();
    let bare = buffer_lines(terminal.backend().buffer()).join("\n");
    assert!(bare.contains("Create managed worktree"), "{bare}");
}

/// Only Claude and Codex can receive Mjolnir's delegation tools, so only they
/// show the choice. The box follows the global `[subagents] enabled` setting.
#[test]
fn new_session_wizard_shows_subagent_checkbox_only_for_claude_and_codex() {
    for (profile, visible) in [(0_usize, true), (1, true), (3, false)] {
        let mut configuration = subagent_wizard_config();
        configuration.subagents.enabled = true;
        let mut dashboard = DashboardState::new(configuration, State::default(), BTreeMap::new());
        dashboard.begin_new();
        let Mode::New(wizard) = &mut dashboard.mode else {
            panic!("new wizard")
        };
        wizard.profile = profile;
        wizard.step = WizardStep::Review;
        wizard.project_directory = "/work/main".into();
        assert!(
            wizard.mjolnir_subagents,
            "the global setting is the default"
        );

        let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .unwrap();
        let text = buffer_lines(terminal.backend().buffer()).join("\n");
        assert_eq!(
            text.contains("Use Mjolnir sub-agents"),
            visible,
            "profile {profile}:\n{text}"
        );
    }
}

/// The wizard sends the box's value for Claude and Codex, and `None` for a
/// harness that cannot receive the tools at all.
#[test]
fn new_session_wizard_sends_subagent_choice() {
    let submit = |profile: usize, toggle: bool| {
        let mut dashboard =
            DashboardState::new(subagent_wizard_config(), State::default(), BTreeMap::new());
        dashboard.begin_new();
        let Mode::New(wizard) = &mut dashboard.mode else {
            panic!("new wizard")
        };
        wizard.profile = profile;
        wizard.step = WizardStep::Review;
        wizard.project_directory = "/work/main".into();
        dashboard.apply_resolved_project_directory(
            &dashboard.path_input_context(),
            "/work/main",
            Ok((
                PathBuf::from("/work/main"),
                mj_core::state::ManagedWorktreeOptions {
                    available: true,
                    default_create: false,
                },
            )),
        );
        // Draw the review step so its controls are declared, as the terminal
        // does before any key reaches them.
        let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .unwrap();
        if toggle {
            let Mode::New(wizard) = &mut dashboard.mode else {
                panic!("new wizard")
            };
            wizard.form.get_mut().focus(WizardControl::MjolnirSubagents);
            ready_key(&mut dashboard, key(KeyCode::Char(' ')));
        }
        let Mode::New(wizard) = &mut dashboard.mode else {
            panic!("new wizard")
        };
        wizard.form.get_mut().focus(WizardControl::Submit);
        ready_key(&mut dashboard, key(KeyCode::Enter))
    };

    assert!(matches!(
        submit(0, false),
        DashboardAction::CreateSession {
            mjolnir_subagents: Some(true),
            ..
        }
    ));
    assert!(matches!(
        submit(0, true),
        DashboardAction::CreateSession {
            mjolnir_subagents: Some(false),
            ..
        }
    ));
    assert!(matches!(
        submit(1, true),
        DashboardAction::CreateSession {
            mjolnir_subagents: Some(false),
            ..
        }
    ));
    // Grok never receives the tools, so the wizard expresses no opinion.
    assert!(matches!(
        submit(3, false),
        DashboardAction::CreateSession {
            mjolnir_subagents: None,
            ..
        }
    ));
}

/// A bare local target plus a Grok profile, so the sub-agent checkbox can be
/// exercised against a harness that never receives the tools.
fn subagent_wizard_config() -> mj_core::config::Config {
    let mut configuration = config();
    configuration.targets.clear();
    configuration
        .targets
        .insert("local".into(), TargetTemplate::LocalBare);
    configuration.profiles.insert(
        "grok-1".into(),
        HarnessProfile {
            enabled: true,
            context_window_bytes: None,
            guardian_review_model: None,
            kind: HarnessKind::Grok,
            home: PathBuf::from("/profiles/grok"),
            environment: BTreeMap::new(),
        },
    );
    configuration
}

#[test]
fn worktree_inspection_ignores_old_directories_and_allows_linked_checkout_opt_in() {
    let mut configuration = config();
    configuration.targets.clear();
    configuration
        .targets
        .insert("local".into(), TargetTemplate::LocalBare);
    let mut dashboard = DashboardState::new(configuration, State::default(), BTreeMap::new());
    dashboard.begin_new();
    let Mode::New(wizard) = &mut dashboard.mode else {
        panic!("new wizard")
    };
    wizard.step = WizardStep::Review;
    wizard.project_directory = "/work/main".into();
    let old_context = dashboard.path_input_context();
    let Mode::New(wizard) = &mut dashboard.mode else {
        panic!("new wizard")
    };
    wizard.project_directory = "/work/linked".into();
    dashboard.apply_resolved_project_directory(
        &old_context,
        "/work/main",
        Ok((
            PathBuf::from("/work/main"),
            mj_core::state::ManagedWorktreeOptions {
                available: true,
                default_create: true,
            },
        )),
    );
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("new wizard")
    };
    assert!(wizard.worktree_options.is_none());
    dashboard.apply_resolved_project_directory(
        &dashboard.path_input_context(),
        "/work/linked",
        Ok((
            PathBuf::from("/work/linked"),
            mj_core::state::ManagedWorktreeOptions {
                available: true,
                default_create: false,
            },
        )),
    );
    let Mode::New(wizard) = &mut dashboard.mode else {
        panic!("new wizard")
    };
    assert!(!wizard.create_managed_worktree);
    wizard
        .form
        .get_mut()
        .focus(WizardControl::CreateManagedWorktree);
    ready_key(&mut dashboard, key(KeyCode::Char(' ')));
    let Mode::New(wizard) = &mut dashboard.mode else {
        panic!("new wizard")
    };
    wizard.form.get_mut().focus(WizardControl::Submit);
    assert!(matches!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::CreateSession {
            create_managed_worktree: Some(true),
            ..
        }
    ));
}

/// Complete availability probes as a healthy runtime would for wizard tests
/// concerned with other behavior. Readiness-specific tests use handle_key directly.
/// Opens the full new-session wizard through its live binding, settling the
/// target readiness probes on either side the way `ready_key` does.
fn ready_open_new_wizard(dashboard: &mut DashboardState) -> DashboardAction {
    complete_ready_targets(dashboard);
    let action = open_new_session_wizard(dashboard);
    complete_ready_targets(dashboard);
    action
}

fn ready_key(dashboard: &mut DashboardState, event: crossterm::event::KeyEvent) -> DashboardAction {
    complete_ready_targets(dashboard);
    let action = dashboard.handle_key(event);
    complete_ready_targets(dashboard);
    action
}

fn complete_ready_targets(dashboard: &mut DashboardState) {
    let on_target = matches!(&dashboard.mode, Mode::New(wizard) if wizard.step == WizardStep::Target)
        || matches!(&dashboard.mode, Mode::Resume(wizard) if wizard.step == WizardStep::Target);
    if on_target
        && let Some(DashboardAction::CheckTargetReadiness {
            generation,
            target_ids,
        }) = dashboard.take_prerequisite_check()
    {
        for id in target_ids {
            dashboard.apply_target_readiness(generation, id, Ok(()));
        }
    }
}

#[test]
fn unavailable_target_blocks_launch_and_refresh_allows_recovery() {
    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    dashboard.begin_new();
    dashboard.handle_key(key(KeyCode::Enter));
    let Some(DashboardAction::CheckTargetReadiness {
        generation,
        target_ids,
    }) = dashboard.take_prerequisite_check()
    else {
        panic!("availability check must start");
    };
    assert_eq!(target_ids, ["podman"]);
    dashboard.handle_key(key(KeyCode::Enter));
    assert!(matches!(&dashboard.mode, Mode::New(wizard) if wizard.step == WizardStep::Target));
    dashboard.apply_target_readiness(
        generation,
        "podman".into(),
        Err("service is stopped".into()),
    );
    let mut terminal = Terminal::new(TestBackend::new(180, 32)).unwrap();
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .unwrap();
    let text = buffer_lines(terminal.backend().buffer()).join("\n");
    assert!(text.contains("unavailable: service is stopped"), "{text}");
    dashboard.handle_key(key(KeyCode::Enter));
    assert!(matches!(&dashboard.mode, Mode::New(wizard) if wizard.step == WizardStep::Target));
    chord(&mut dashboard, crate::CommandId::Refresh);
    let Some(DashboardAction::CheckTargetReadiness {
        generation: fresh, ..
    }) = dashboard.take_prerequisite_check()
    else {
        panic!("refresh must recheck");
    };
    dashboard.apply_target_readiness(generation, "podman".into(), Ok(()));
    assert!(dashboard.target_readiness_rejection("podman").is_some());
    dashboard.apply_target_readiness(fresh, "podman".into(), Ok(()));
    dashboard.handle_key(key(KeyCode::Enter));
    assert!(matches!(&dashboard.mode, Mode::New(wizard) if wizard.step == WizardStep::Bundle));
}

#[test]
fn readiness_checks_are_independent_and_ignore_changed_configuration() {
    let mut config = config();
    config.targets.insert(
        "remote".into(),
        TargetTemplate::SshBare {
            ssh: SshConnection {
                host: "example.test".into(),
                user: None,
                identity_file: None,
                extra_args: vec![],
            },
            permissions: mj_core::config::PermissionMode::Guardian,
            workspace_prefix: PathBuf::from("workspaces"),
        },
    );
    let mut dashboard = DashboardState::new(config, State::default(), BTreeMap::new());
    dashboard.begin_new();
    dashboard.handle_key(key(KeyCode::Enter));
    let Some(DashboardAction::CheckTargetReadiness {
        generation,
        target_ids,
    }) = dashboard.take_prerequisite_check()
    else {
        panic!("check");
    };
    assert_eq!(target_ids.len(), 2);
    dashboard.apply_target_readiness(generation, "remote".into(), Ok(()));
    assert!(dashboard.target_readiness_rejection("remote").is_none());
    assert!(dashboard.target_readiness_rejection("podman").is_some());
    if let TargetTemplate::SshBare { ssh, .. } = dashboard.config.targets.get_mut("remote").unwrap()
    {
        ssh.host = "changed.test".into();
    }
    dashboard.apply_target_readiness(generation, "remote".into(), Ok(()));
    assert!(dashboard.target_readiness_rejection("remote").is_some());
    dashboard.handle_key(key(KeyCode::Esc));
    assert!(!matches!(dashboard.mode, Mode::New(_)));
}

/// Reopening the wizard with the target's template unchanged must reuse the
/// readiness result from the previous open instead of re-probing: the probe
/// is an ssh round trip for non-local targets, so a second open in the same
/// dashboard session should be instant.
#[test]
fn reopening_wizard_with_unchanged_template_reuses_fresh_readiness() {
    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    dashboard.begin_new();
    dashboard.handle_key(key(KeyCode::Enter));
    let Some(DashboardAction::CheckTargetReadiness {
        generation,
        target_ids,
    }) = dashboard.take_prerequisite_check()
    else {
        panic!("the first open must probe the non-local target");
    };
    assert_eq!(target_ids, ["podman"]);
    dashboard.apply_target_readiness(generation, "podman".into(), Ok(()));
    assert!(dashboard.target_readiness_rejection("podman").is_none());

    dashboard.handle_key(key(KeyCode::Esc));
    assert!(!matches!(dashboard.mode, Mode::New(_)));

    dashboard.begin_new();
    dashboard.handle_key(key(KeyCode::Enter));
    assert_eq!(
        dashboard.take_prerequisite_check(),
        None,
        "a fresh readiness result from the previous open must not be re-probed"
    );
    assert!(dashboard.target_readiness_rejection("podman").is_none());
}

/// A target's template changing between wizard opens invalidates the cached
/// readiness result even though it has not gone stale.
#[test]
fn reopening_wizard_after_template_change_reprobes() {
    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    dashboard.begin_new();
    dashboard.handle_key(key(KeyCode::Enter));
    let Some(DashboardAction::CheckTargetReadiness {
        generation,
        target_ids,
    }) = dashboard.take_prerequisite_check()
    else {
        panic!("the first open must probe the non-local target");
    };
    assert_eq!(target_ids, ["podman"]);
    dashboard.apply_target_readiness(generation, "podman".into(), Ok(()));
    dashboard.handle_key(key(KeyCode::Esc));
    assert!(!matches!(dashboard.mode, Mode::New(_)));

    match dashboard.config.targets.get_mut("podman").unwrap() {
        TargetTemplate::LocalPodman { container } => {
            container.image = "ubuntu:22.04".into();
        }
        other => panic!("expected a podman target, got {other:?}"),
    }

    dashboard.begin_new();
    dashboard.handle_key(key(KeyCode::Enter));
    let Some(DashboardAction::CheckTargetReadiness { target_ids, .. }) =
        dashboard.take_prerequisite_check()
    else {
        panic!("a changed template must be re-probed even though a result is cached");
    };
    assert_eq!(target_ids, ["podman"]);
}

/// A readiness result older than [`TARGET_READINESS_TTL`] is treated as
/// missing: the wizard shows the checking state again and rejects Create
/// until a fresh probe completes.
#[test]
fn stale_readiness_result_is_treated_as_missing_and_reprobes() {
    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    dashboard.begin_new();
    dashboard.handle_key(key(KeyCode::Enter));
    let Some(DashboardAction::CheckTargetReadiness {
        generation,
        target_ids,
    }) = dashboard.take_prerequisite_check()
    else {
        panic!("the first open must probe the non-local target");
    };
    assert_eq!(target_ids, ["podman"]);
    dashboard.apply_target_readiness(generation, "podman".into(), Ok(()));
    assert!(dashboard.target_readiness_rejection("podman").is_none());

    dashboard
        .target_readiness
        .get_mut("podman")
        .expect("readiness entry was recorded")
        .recorded_at = Instant::now() - TARGET_READINESS_TTL;

    assert_eq!(
        dashboard.target_readiness_rejection("podman"),
        Some("checking availability…".into()),
        "a stale result must reject exactly like a missing one"
    );
    let Some(DashboardAction::CheckTargetReadiness { target_ids, .. }) =
        dashboard.take_prerequisite_check()
    else {
        panic!("a stale result must be re-probed");
    };
    assert_eq!(target_ids, ["podman"]);
}

/// A failed result is kept only briefly: a failure is usually a host that is
/// asleep or a probe that timed out, which the user fixes and retries within
/// minutes, so it is re-probed long before a success would be.
#[test]
fn failed_readiness_result_is_reprobed_after_the_short_failure_ttl() {
    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    dashboard.begin_new();
    dashboard.handle_key(key(KeyCode::Enter));
    let Some(DashboardAction::CheckTargetReadiness { generation, .. }) =
        dashboard.take_prerequisite_check()
    else {
        panic!("the first open must probe the non-local target");
    };
    dashboard.apply_target_readiness(generation, "podman".into(), Err("host asleep".into()));
    assert_eq!(
        dashboard.target_readiness_rejection("podman"),
        Some("unavailable: host asleep".into())
    );
    assert!(
        dashboard.take_prerequisite_check().is_none(),
        "a fresh failure is not re-probed on its own"
    );

    dashboard
        .target_readiness
        .get_mut("podman")
        .expect("readiness entry was recorded")
        .recorded_at = Instant::now() - TARGET_READINESS_FAILURE_TTL;

    assert_eq!(
        dashboard.target_readiness_rejection("podman"),
        Some("checking availability…".into()),
        "an aged failure must reject like a missing entry, not repeat the old error"
    );
    let Some(DashboardAction::CheckTargetReadiness { target_ids, .. }) =
        dashboard.take_prerequisite_check()
    else {
        panic!("an aged failure must be re-probed");
    };
    assert_eq!(target_ids, ["podman"]);
}
