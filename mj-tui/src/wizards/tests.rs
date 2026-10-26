use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use mj_core::config::{HarnessKind, HarnessProfile, SshConnection, TargetTemplate};
use mj_core::path_completion::{CompletionHost, CompletionKind, PathCompletion};
use mj_core::project_picker::{
    ProjectDiscovery, ProjectDiscoveryRequest, ProjectEntry, ProjectEntryKind,
};
use mj_core::state::{HostContainerSize, SessionResourceAllocation, State};

use mj_core::targets::{AdditionalMount, MountAccess};

use mj_client::quota::{ProfileQuota, QuotaWindow};

use super::*;
use crate::test_support::*;

use crate::render::render;
use crate::{DashboardAction, DashboardState, Mode, nth_key};

#[derive(Default)]
struct GoldenCapture {
    label: Option<String>,
    last_action: Option<String>,
    output: String,
}

thread_local! {
    static GOLDEN_CAPTURE: RefCell<GoldenCapture> = RefCell::default();
}

fn capture_scenario(label: &str, run: impl FnOnce()) {
    GOLDEN_CAPTURE.with(|capture| {
        let mut capture = capture.borrow_mut();
        capture.label = Some(label.to_owned());
        capture.last_action = None;
    });
    run();
    GOLDEN_CAPTURE.with(|capture| capture.borrow_mut().label = None);
}

fn capture_action(action: &DashboardAction) {
    GOLDEN_CAPTURE.with(|capture| {
        let mut capture = capture.borrow_mut();
        if capture.label.is_none() {
            return;
        }
        let action = format!("{action:?}");
        if capture.last_action.as_deref() == Some(&action) {
            return;
        }
        capture.output.push_str(&format!("action: {action}\n"));
        capture.last_action = Some(action);
    });
}

fn capture_detail(label: &str, value: &str) {
    GOLDEN_CAPTURE.with(|capture| {
        let mut capture = capture.borrow_mut();
        if capture.label.is_some() {
            capture.output.push_str(&format!("{label}: {value}\n"));
        }
    });
}

fn capture_render(width: u16, height: u16, lines: &[String]) {
    GOLDEN_CAPTURE.with(|capture| {
        let mut capture = capture.borrow_mut();
        if let Some(label) = capture.label.clone() {
            capture.output.push_str(&format!(
                "=== {label} ({width}x{height}) ===\n{}\n",
                lines.join("\n")
            ));
        }
    });
}

fn set_local_container_capacity(
    dashboard: &mut DashboardState,
    total: HostContainerSize,
    committed: HostContainerSize,
    remaining: HostContainerSize,
    complete: bool,
) {
    let reserved = mj_core::state::reserved_container_capacity(total);
    dashboard.set_container_capacities(BTreeMap::from([(
        "local".into(),
        mj_client::runtime_feed::RuntimeContainerCapacity {
            target_ids: vec!["podman".into()],
            capacity: mj_core::state::HostContainerCapacity {
                total,
                reserved,
                committed,
                remaining,
                complete,
            },
        },
    )]));
}

fn captured_drawn(dashboard: &mut DashboardState, width: u16, height: u16) -> Vec<String> {
    let lines = crate::test_support::drawn(dashboard, width, height);
    capture_render(width, height, &lines);
    lines
}

fn take_golden_output() -> String {
    GOLDEN_CAPTURE.with(|capture| std::mem::take(&mut capture.borrow_mut().output))
}

// Hard-won: 93d7fde8: New review could not finish because only disabled Create started preflight
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
fn pending_creation_preflight_keeps_back_and_cancel_live_and_ignores_late_results() {
    for control in [WizardControl::Back, WizardControl::Cancel] {
        let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
        ready_open_new_wizard(&mut dashboard);
        for _ in 0..3 {
            ready_key(&mut dashboard, key(KeyCode::Enter));
        }
        assert!(matches!(
            dashboard.take_prerequisite_check(),
            Some(DashboardAction::PreflightCreateSession { .. })
        ));
        let generation = dashboard.session_preflight_generation();
        dashboard.begin_remote_session_preflight(generation);
        let Mode::New(wizard) = &mut dashboard.mode else {
            panic!("new wizard")
        };
        wizard.form.get_mut().focus(control);
        ready_key(&mut dashboard, key(KeyCode::Enter));
        dashboard.apply_remote_session_preflight(generation, Err("late failure".into()));
        match control {
            WizardControl::Back => {
                let Mode::New(wizard) = &dashboard.mode else {
                    panic!("new wizard")
                };
                assert_eq!(wizard.step, WizardStep::Bundle);
                assert!(!wizard.remote_preflight_in_flight);
                assert!(wizard.remote_preflight_error.is_none());
            }
            WizardControl::Cancel => assert!(matches!(dashboard.mode, Mode::Dashboard)),
            _ => unreachable!(),
        }
    }
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

fn dashboard_at_project_picker() -> DashboardState {
    let mut configuration = config();
    configuration.bundles.clear();
    let mut dashboard = DashboardState::new(configuration, State::default(), BTreeMap::new());
    ready_open_new_wizard(&mut dashboard);
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    assert_eq!(project_wizard(&dashboard).step, WizardStep::NewBundle);
    dashboard
}

fn project_wizard(dashboard: &DashboardState) -> &NewWizard {
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected the new-session wizard");
    };
    wizard
}

fn focus_project_control(dashboard: &mut DashboardState, control: WizardControl) {
    let Mode::New(wizard) = &mut dashboard.mode else {
        panic!("expected the new-session wizard");
    };
    wizard.form.get_mut().focus(control);
}

fn activate_project_control(
    dashboard: &mut DashboardState,
    control: WizardControl,
) -> DashboardAction {
    focus_project_control(dashboard, control);
    ready_key(dashboard, key(KeyCode::Enter))
}

fn type_source(dashboard: &mut DashboardState, source: &str) {
    for character in source.chars() {
        ready_key(dashboard, key(KeyCode::Char(character)));
    }
}

fn take_project_request(dashboard: &mut DashboardState) -> (String, ProjectDiscoveryRequest) {
    let Some(DashboardAction::DiscoverProjects { context, request }) =
        dashboard.take_project_discovery()
    else {
        panic!("expected a pending project discovery request");
    };
    assert_eq!(dashboard.take_project_discovery(), None);
    capture_detail("project request", &format!("{request:?}"));
    (context, request)
}

fn discovered_repository(source: &str) -> ProjectEntry {
    ProjectEntry {
        name: source.rsplit('/').next().unwrap().into(),
        source: source.into(),
        description: source.into(),
        kind: ProjectEntryKind::Repository,
    }
}

fn discovered_projects(entries: Vec<ProjectEntry>) -> ProjectDiscovery {
    ProjectDiscovery {
        entries,
        directory: None,
        parent: None,
        truncated: false,
    }
}

fn draw_project_picker(
    dashboard: &mut DashboardState,
    terminal: &mut Terminal<TestBackend>,
) -> Vec<String> {
    dashboard.reset_component_geometry();
    terminal.draw(|frame| render(frame, dashboard)).unwrap();
    let lines = buffer_lines(terminal.backend().buffer());
    let area = terminal.backend().buffer().area;
    capture_render(area.width, area.height, &lines);
    lines
}

fn click_project_text(
    dashboard: &mut DashboardState,
    terminal: &mut Terminal<TestBackend>,
    label: &str,
) -> DashboardAction {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let lines = draw_project_picker(dashboard, terminal);
    let (row, column) = lines
        .iter()
        .enumerate()
        .find_map(|(row, line)| {
            line.find(label)
                .map(|column| (row as u16, line[..column].chars().count() as u16))
        })
        .unwrap_or_else(|| panic!("missing clickable {label:?}:\n{}", lines.join("\n")));
    let mut action = DashboardAction::None;
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        let next = dashboard.handle_mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        });
        capture_action(&next);
        if next != DashboardAction::None {
            assert_eq!(action, DashboardAction::None, "one click submits only once");
            action = next;
        }
        draw_project_picker(dashboard, terminal);
    }
    action
}

fn project_picker_skips_empty_saved_projects_and_offers_github_from_recent() {
    let mut dashboard = dashboard_at_project_picker();
    let wizard = project_wizard(&dashboard);
    assert_eq!(wizard.project_picker.tab, ProjectTab::Recent);
    assert_eq!(
        wizard.form.borrow().focused(),
        Some(WizardControl::ProjectGithub)
    );
    assert!(!wizard.project_picker.multiple);
    let discovery = dashboard.take_project_discovery();
    if let Some(action) = discovery.as_ref() {
        capture_action(action);
    }
    assert_eq!(discovery, Some(DashboardAction::LoadMountHistory));
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let text = draw_project_picker(&mut dashboard, &mut terminal).join("\n");
    assert!(text.contains("Choose a project"), "{text}");
    assert!(text.contains("No recent projects yet"), "{text}");
    assert!(!text.contains("No bundles yet"), "{text}");
    assert!(!text.contains("Add repository"), "{text}");
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert_eq!(
        take_project_request(&mut dashboard).1,
        ProjectDiscoveryRequest::Github {
            query: String::new()
        }
    );
    assert_eq!(
        activate_project_control(&mut dashboard, WizardControl::Back),
        DashboardAction::None
    );
    assert_eq!(project_wizard(&dashboard).step, WizardStep::Target);
}

/// A saved-projects fixture: `hel` from the shared config, plus a second
/// project with two repositories, one of them a path too long for a row.
fn two_project_config() -> mj_core::config::Config {
    let mut configuration = config();
    let long_path =
        "/tmp/claude-1000/-home-jonathan-Projects-hel/b295cc91-b89b-4bf0-9d1a-0123456789ab/bifrost";
    configuration.bundles.insert(
        "bifrost2".into(),
        mj_core::config::ProjectBundle {
            primary_repo: "bifrost-dev".into(),
            repositories: vec![
                mj_core::config::ProjectRepository {
                    id: "bifrost-dev".into(),
                    github: Some("BrokkAi/bifrost-dev".into()),
                    local: None,
                    destination: PathBuf::from("bifrost-dev"),
                    git_ref: None,
                },
                mj_core::config::ProjectRepository {
                    id: "bifrost".into(),
                    github: None,
                    local: Some(PathBuf::from(long_path)),
                    destination: PathBuf::from("bifrost"),
                    git_ref: None,
                },
            ],
        },
    );
    configuration
}

fn dashboard_at_saved_projects(state: State) -> DashboardState {
    let mut dashboard = DashboardState::new(two_project_config(), state, BTreeMap::new());
    ready_open_new_wizard(&mut dashboard);
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    assert_eq!(project_wizard(&dashboard).step, WizardStep::Bundle);
    dashboard
}

fn select_saved_project(dashboard: &mut DashboardState, bundle_id: &str) {
    let index = bundle_ids_by_recent_creation(&dashboard.config, &dashboard.launch_recency)
        .iter()
        .position(|id| *id == bundle_id)
        .unwrap();
    let Mode::New(wizard) = &mut dashboard.mode else {
        panic!("expected the new-session wizard");
    };
    wizard.bundle = index;
    wizard.form.get_mut().focus(WizardControl::BundleList);
}

/// A configured project whose local directory is gone stays listed, marked
/// unavailable with the reason. Finding T-6.
// Hard-won: d07fc1f0: missing saved project directories appeared usable and leaked raw git errors
#[test]
fn a_saved_project_with_a_missing_directory_is_marked_unavailable() {
    let missing = PathBuf::from("/definitely/not/here/missingpath");
    let mut configuration = config();
    configuration.bundles.insert(
        "missingpath".into(),
        mj_core::config::ProjectBundle {
            primary_repo: "missingpath".into(),
            repositories: vec![mj_core::config::ProjectRepository {
                id: "missingpath".into(),
                github: None,
                local: Some(missing.clone()),
                destination: PathBuf::from("missingpath"),
                git_ref: None,
            }],
        },
    );
    let mut dashboard = DashboardState::new(configuration, State::default(), BTreeMap::new());
    // The dashboard loop asks for checks after every event; collect them
    // while moving to the project step.
    let mut checks = Vec::new();
    let mut collect = |dashboard: &mut DashboardState| {
        while let Some(action) = dashboard.take_prerequisite_check() {
            checks.push(action);
        }
    };
    open_new_session_wizard(&mut dashboard);
    collect(&mut dashboard);
    for _ in 0..2 {
        dashboard.handle_key(key(KeyCode::Enter));
        collect(&mut dashboard);
    }
    assert_eq!(project_wizard(&dashboard).step, WizardStep::Bundle);
    assert_eq!(
        checks,
        vec![DashboardAction::CheckProjectDirectories {
            paths: vec![missing.clone()]
        }],
        "the directory is checked once, off the draw loop"
    );
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    let before = draw_project_picker(&mut dashboard, &mut terminal).join("\n");
    assert!(before.contains("missingpath"), "{before}");
    assert!(!before.contains("unavailable"), "{before}");

    // The answer marks the row.
    dashboard.apply_project_directory_check(missing, Some(false));
    select_saved_project(&mut dashboard, "missingpath");
    let lines = draw_project_picker(&mut dashboard, &mut terminal);
    let text = lines.join("\n");
    assert!(
        lines
            .iter()
            .any(|line| line.contains("missingpath") && line.contains("(unavailable")),
        "{text}"
    );
    let joined = lines
        .iter()
        .map(|line| line.trim_matches(|c: char| c == '│' || c.is_whitespace()))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        joined.contains("Unavailable: the directory") && joined.contains("does not exist."),
        "{text}"
    );
}

fn saved_projects_show_every_source_of_the_selection_and_stack_add_and_remove() {
    let mut dashboard = dashboard_at_saved_projects(State::default());
    select_saved_project(&mut dashboard, "bifrost2");
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    let lines = draw_project_picker(&mut dashboard, &mut terminal);
    let text = lines.join("\n");
    assert!(text.contains("choose a project"), "{text}");
    // Rows name the first source and count the rest.
    assert!(
        lines
            .iter()
            .any(|line| line.contains("bifrost2") && line.contains("BrokkAi/bifrost-dev  +1 more")),
        "{text}"
    );
    // The details under the list spell out every source of the selection,
    // wrapped rather than cut, and name the primary repository.
    assert!(text.contains("bifrost2 · 2 repositories"), "{text}");
    assert!(text.contains("BrokkAi/bifrost-dev  (primary)"), "{text}");
    let joined = lines
        .iter()
        .map(|line| line.trim_matches(|c: char| c == '│' || c.is_whitespace()))
        .collect::<String>();
    assert!(joined.contains("0123456789ab/bifrost"), "{text}");
    // Add and Remove stack at the upper right, apart from the navigation row.
    let row_of = |label: &str| {
        lines
            .iter()
            .position(|line| line.contains(label))
            .unwrap_or_else(|| panic!("missing {label:?}:\n{text}"))
    };
    let (add, remove, cancel) = (row_of("Add…"), row_of("Remove"), row_of("Cancel"));
    assert_eq!(remove, add + 1, "{text}");
    assert!(add < row_of("bifrost2 ·") && cancel > remove, "{text}");
    assert!(!lines[cancel].contains("Add"), "{text}");
    assert_eq!(
        click_project_text(&mut dashboard, &mut terminal, "Add…"),
        DashboardAction::None
    );
    assert_eq!(project_wizard(&dashboard).step, WizardStep::NewBundle);
    activate_project_control(&mut dashboard, WizardControl::Back);
    assert_eq!(project_wizard(&dashboard).step, WizardStep::Bundle);
    draw_project_picker(&mut dashboard, &mut terminal);
}

#[test]
fn removing_a_saved_project_refuses_one_a_session_uses_and_keeps_a_valid_selection() {
    let mut state = State::default();
    let session = stopped_session();
    state.sessions.insert(session.id.clone(), session);
    let mut dashboard = dashboard_at_saved_projects(state);

    // A suspended session still resumes from `hel`, so Delete explains why
    // it stays instead of removing it.
    select_saved_project(&mut dashboard, "hel");
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Delete)),
        DashboardAction::None
    );
    let notice = dashboard.notice().unwrap();
    assert!(notice.contains("\"hel\" is used by a session"), "{notice}");
    assert!(notice.contains("ACP pretty name"), "{notice}");
    assert!(!project_wizard(&dashboard).bundle_removal_in_flight);

    select_saved_project(&mut dashboard, "bifrost2");
    assert_eq!(
        activate_project_control(&mut dashboard, WizardControl::RemoveBundle),
        DashboardAction::RemoveBundle {
            bundle_id: "bifrost2".into()
        }
    );
    assert!(project_wizard(&dashboard).bundle_removal_in_flight);
    // The list is frozen until the config write is confirmed.
    for code in [KeyCode::Delete, KeyCode::Enter, KeyCode::Up] {
        assert_eq!(ready_key(&mut dashboard, key(code)), DashboardAction::None);
    }

    let mut removed = dashboard.config.clone();
    removed.bundles.remove("bifrost2");
    dashboard.apply_removed_bundle(removed, "bifrost2");
    let wizard = project_wizard(&dashboard);
    assert!(!wizard.bundle_removal_in_flight);
    assert_eq!(wizard.step, WizardStep::Bundle);
    assert_eq!(
        nth_bundle_key(&dashboard.config, &dashboard.launch_recency, wizard.bundle),
        "hel"
    );
    assert_eq!(
        dashboard.notice().as_deref(),
        Some("Removed project bifrost2.")
    );
}

fn project_picker_url_enter_creates_one_repository_and_advances_to_review() {
    let mut dashboard = dashboard_at_project_picker();
    activate_project_control(&mut dashboard, WizardControl::ProjectUrl);
    type_source(&mut dashboard, "owner/only");
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::CreateBundle {
            sources: vec!["owner/only".into()]
        }
    );
    assert!(project_wizard(&dashboard).bundle_creation_in_flight);
    for code in [KeyCode::Enter, KeyCode::Esc, KeyCode::Backspace] {
        assert_eq!(ready_key(&mut dashboard, key(code)), DashboardAction::None);
    }
    assert_eq!(project_wizard(&dashboard).new_bundle_source, "owner/only");
    let created = config();
    let id = created.bundles.keys().next().unwrap().clone();
    dashboard.apply_created_bundle(created, &id);
    captured_drawn(&mut dashboard, 100, 30);
    let wizard = project_wizard(&dashboard);
    assert_eq!(wizard.step, WizardStep::Review);
    assert!(!wizard.bundle_creation_in_flight);
    assert_eq!(
        nth_bundle_key(&dashboard.config, &dashboard.launch_recency, wizard.bundle),
        id
    );
}

#[test]
fn project_picker_creation_failure_preserves_source_for_retry() {
    let mut dashboard = dashboard_at_project_picker();
    activate_project_control(&mut dashboard, WizardControl::ProjectUrl);
    type_source(&mut dashboard, "owner/only");
    let expected = DashboardAction::CreateBundle {
        sources: vec!["owner/only".into()],
    };
    assert_eq!(ready_key(&mut dashboard, key(KeyCode::Enter)), expected);
    dashboard.fail_bundle_creation("repository not found");
    let wizard = project_wizard(&dashboard);
    assert_eq!(wizard.step, WizardStep::NewBundle);
    assert_eq!(wizard.project_picker.tab, ProjectTab::Url);
    assert!(!wizard.bundle_creation_in_flight);
    assert_eq!(wizard.new_bundle_source, "owner/only");
    assert!(dashboard.notice().unwrap().contains("repository not found"));
    assert_eq!(ready_key(&mut dashboard, key(KeyCode::Enter)), expected);
}

#[test]
fn project_picker_creation_retry_resubmits_selected_repository_without_rediscovery() {
    let mut dashboard = dashboard_at_project_picker();
    activate_project_control(&mut dashboard, WizardControl::ProjectGithub);
    let (context, _) = take_project_request(&mut dashboard);
    dashboard.apply_project_discovery(
        &context,
        Ok(discovered_projects(vec![discovered_repository(
            "owner/app",
        )])),
    );
    let expected = DashboardAction::CreateBundle {
        sources: vec!["owner/app".into()],
    };
    assert_eq!(
        activate_project_control(&mut dashboard, WizardControl::ProjectResults),
        expected
    );
    dashboard.fail_bundle_creation("Could not save the selected project");
    let wizard = project_wizard(&dashboard);
    assert!(wizard.project_picker.error.is_none());
    assert!(!wizard.bundle_creation_in_flight);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let text = draw_project_picker(&mut dashboard, &mut terminal).join("\n");
    assert!(
        text.contains("Could not save the selected project"),
        "{text}"
    );
    assert_eq!(
        click_project_text(&mut dashboard, &mut terminal, "Retry"),
        expected
    );
    assert!(project_wizard(&dashboard).bundle_creation_in_flight);
    assert_eq!(dashboard.take_project_discovery(), None);
}

#[test]
fn project_picker_turning_multiple_off_keeps_chosen_member_and_rejects_pending_results() {
    let mut dashboard = dashboard_at_project_picker();
    activate_project_control(&mut dashboard, WizardControl::ProjectUrl);
    focus_project_control(&mut dashboard, WizardControl::ProjectMultiple);
    ready_key(&mut dashboard, key(KeyCode::Char(' ')));
    focus_project_control(&mut dashboard, WizardControl::NewBundleSource);
    for source in ["owner/keep", "owner/other"] {
        type_source(&mut dashboard, source);
        ready_key(&mut dashboard, key(KeyCode::Enter));
    }
    focus_project_control(&mut dashboard, WizardControl::NewBundleRepositories);
    ready_key(&mut dashboard, key(KeyCode::Up));
    activate_project_control(&mut dashboard, WizardControl::ProjectGithub);
    let (context, _) = take_project_request(&mut dashboard);
    focus_project_control(&mut dashboard, WizardControl::ProjectMultiple);
    ready_key(&mut dashboard, key(KeyCode::Char(' ')));
    let wizard = project_wizard(&dashboard);
    assert!(!wizard.project_picker.multiple);
    assert_eq!(wizard.project_picker.tab, ProjectTab::Url);
    assert_eq!(wizard.new_bundle_source, "owner/keep");
    assert!(wizard.new_bundle_repositories.is_empty());
    dashboard.apply_project_discovery(&context, Err("obsolete GitHub failure".into()));
    assert!(project_wizard(&dashboard).project_picker.error.is_none());
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::CreateBundle {
            sources: vec!["owner/keep".into()],
        }
    );
}

fn project_picker_multiple_sources_wait_for_submit_and_remove_selected() {
    let mut dashboard = dashboard_at_project_picker();
    activate_project_control(&mut dashboard, WizardControl::ProjectUrl);
    focus_project_control(&mut dashboard, WizardControl::ProjectMultiple);
    ready_key(&mut dashboard, key(KeyCode::Char(' ')));
    assert!(project_wizard(&dashboard).project_picker.multiple);
    focus_project_control(&mut dashboard, WizardControl::NewBundleSource);
    for source in ["owner/main", "owner/remove", "owner/shared"] {
        type_source(&mut dashboard, source);
        assert_eq!(
            ready_key(&mut dashboard, key(KeyCode::Enter)),
            DashboardAction::None
        );
    }
    assert_eq!(
        project_wizard(&dashboard).new_bundle_repositories,
        ["owner/main", "owner/remove", "owner/shared"]
    );
    captured_drawn(&mut dashboard, 100, 30);
    assert!(project_wizard(&dashboard).new_bundle_source.is_empty());
    focus_project_control(&mut dashboard, WizardControl::NewBundleRepositories);
    ready_key(&mut dashboard, key(KeyCode::Up));
    ready_key(&mut dashboard, key(KeyCode::Delete));
    assert_eq!(
        project_wizard(&dashboard).new_bundle_repositories,
        ["owner/main", "owner/shared"]
    );
    captured_drawn(&mut dashboard, 100, 30);
    assert_eq!(
        activate_project_control(&mut dashboard, WizardControl::Next),
        DashboardAction::CreateBundle {
            sources: vec!["owner/main".into(), "owner/shared".into()]
        }
    );
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::None
    );
    capture_detail("selection", "keyboard-selected owner/app is submitted");
}

#[test]
fn project_picker_recent_deduplicates_launch_directory_and_uses_only_local_history() {
    let mut configuration = config();
    configuration.bundles.clear();
    let mut state = State::default();
    state.remember_project_directory("local", std::path::Path::new("/work/older"));
    state.remember_project_directory("local", std::path::Path::new("/work/current"));
    state.remember_project_directory("builder", std::path::Path::new("/remote/other"));
    let mut dashboard = DashboardState::new(configuration, state, BTreeMap::new());
    dashboard.set_launch_project_directory(Some(PathBuf::from("/work/current")));
    ready_open_new_wizard(&mut dashboard);
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    let wizard = project_wizard(&dashboard);
    assert_eq!(
        wizard
            .project_picker
            .entries
            .iter()
            .map(|entry| entry.source.as_str())
            .collect::<Vec<_>>(),
        ["/work/current", "/work/older"]
    );
    assert_eq!(
        wizard.form.borrow().focused(),
        Some(WizardControl::ProjectResults)
    );
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let text = draw_project_picker(&mut dashboard, &mut terminal).join("\n");
    assert!(text.contains("Current project"), "{text}");
    assert!(text.contains("/work/older"), "{text}");
    assert!(!text.contains("/remote/other"), "{text}");
    ready_key(&mut dashboard, key(KeyCode::Down));
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::CreateBundle {
            sources: vec!["/work/older".into()]
        }
    );
}

fn project_picker_github_search_chooses_the_keyboard_selected_result() {
    let mut dashboard = dashboard_at_project_picker();
    activate_project_control(&mut dashboard, WizardControl::ProjectGithub);
    let (initial, request) = take_project_request(&mut dashboard);
    assert_eq!(
        request,
        ProjectDiscoveryRequest::Github {
            query: String::new()
        }
    );
    type_source(&mut dashboard, "owner/app");
    ready_key(&mut dashboard, key(KeyCode::Enter));
    let (search, request) = take_project_request(&mut dashboard);
    assert_eq!(
        request,
        ProjectDiscoveryRequest::Github {
            query: "owner/app".into()
        }
    );
    dashboard.apply_project_discovery(
        &initial,
        Ok(discovered_projects(vec![discovered_repository(
            "owner/stale",
        )])),
    );
    assert!(project_wizard(&dashboard).project_picker.entries.is_empty());
    assert!(project_wizard(&dashboard).project_picker.loading);
    dashboard.apply_project_discovery(
        &search,
        Ok(discovered_projects(vec![
            discovered_repository("owner/apple"),
            discovered_repository("owner/app"),
        ])),
    );
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let text = draw_project_picker(&mut dashboard, &mut terminal).join("\n");
    assert!(text.contains("owner/apple"), "{text}");
    assert!(text.contains("owner/app"), "{text}");
    assert!(!text.contains("owner/stale"), "{text}");
    focus_project_control(&mut dashboard, WizardControl::ProjectResults);
    ready_key(&mut dashboard, key(KeyCode::Down));
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::CreateBundle {
            sources: vec!["owner/app".into()]
        }
    );
}

#[test]
fn project_picker_github_result_does_not_overwrite_query_or_steal_edit_focus() {
    let mut dashboard = dashboard_at_project_picker();
    activate_project_control(&mut dashboard, WizardControl::ProjectGithub);
    let (context, _) = take_project_request(&mut dashboard);
    type_source(&mut dashboard, "still typing");
    dashboard.apply_project_discovery(
        &context,
        Ok(discovered_projects(vec![discovered_repository(
            "owner/app",
        )])),
    );
    let wizard = project_wizard(&dashboard);
    assert_eq!(wizard.project_picker.query, "still typing");
    assert_eq!(
        wizard.form.borrow().focused(),
        Some(WizardControl::ProjectQuery)
    );
    type_source(&mut dashboard, " now");
    assert_eq!(
        project_wizard(&dashboard).project_picker.query,
        "still typing now"
    );
}

fn project_picker_folders_navigate_up_home_and_choose_current_repository() {
    let mut dashboard = dashboard_at_project_picker();
    activate_project_control(&mut dashboard, WizardControl::ProjectFolders);
    let (home, request) = take_project_request(&mut dashboard);
    assert_eq!(
        request,
        ProjectDiscoveryRequest::Directory {
            path: String::new(),
            filter: String::new(),
        }
    );
    let home_result = ProjectDiscovery {
        entries: vec![ProjectEntry {
            name: "src".into(),
            source: "/home/user/src".into(),
            description: "Folder".into(),
            kind: ProjectEntryKind::Directory,
        }],
        directory: Some("/home/user".into()),
        parent: Some("/home".into()),
        truncated: false,
    };
    dashboard.apply_project_discovery(&home, Ok(home_result.clone()));
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::None
    );
    let (child, request) = take_project_request(&mut dashboard);
    assert_eq!(
        request,
        ProjectDiscoveryRequest::Directory {
            path: "/home/user/src".into(),
            filter: String::new(),
        }
    );
    let repo_result = ProjectDiscovery {
        entries: vec![ProjectEntry {
            name: "Use src".into(),
            source: "/home/user/src".into(),
            description: "Use this repository".into(),
            kind: ProjectEntryKind::Repository,
        }],
        directory: Some("/home/user/src".into()),
        parent: Some("/home/user".into()),
        truncated: false,
    };
    dashboard.apply_project_discovery(&child, Ok(repo_result.clone()));
    activate_project_control(&mut dashboard, WizardControl::ProjectUp);
    let (up, request) = take_project_request(&mut dashboard);
    assert_eq!(
        request,
        ProjectDiscoveryRequest::Directory {
            path: "/home/user".into(),
            filter: String::new(),
        }
    );
    dashboard.apply_project_discovery(&up, Ok(home_result.clone()));
    activate_project_control(&mut dashboard, WizardControl::ProjectHome);
    let (home, request) = take_project_request(&mut dashboard);
    assert_eq!(
        request,
        ProjectDiscoveryRequest::Directory {
            path: String::new(),
            filter: String::new(),
        }
    );
    dashboard.apply_project_discovery(&home, Ok(home_result));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    let (child, _) = take_project_request(&mut dashboard);
    dashboard.apply_project_discovery(&child, Ok(repo_result));
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let text = draw_project_picker(&mut dashboard, &mut terminal).join("\n");
    assert!(text.contains("/home/user/src"), "{text}");
    assert!(text.contains("Use this repository"), "{text}");
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::CreateBundle {
            sources: vec!["/home/user/src".into()]
        }
    );
}

#[test]
fn project_picker_folder_filter_retry_and_open_repository_preserve_navigation() {
    let mut dashboard = dashboard_at_project_picker();
    activate_project_control(&mut dashboard, WizardControl::ProjectFolders);
    let (initial, _) = take_project_request(&mut dashboard);
    dashboard.apply_project_discovery(
        &initial,
        Ok(ProjectDiscovery {
            entries: vec![
                discovered_repository("/work/app"),
                discovered_repository("/work/shared"),
            ],
            directory: Some("/work".into()),
            parent: Some("/".into()),
            truncated: false,
        }),
    );
    focus_project_control(&mut dashboard, WizardControl::ProjectQuery);
    type_source(&mut dashboard, "app");
    ready_key(&mut dashboard, key(KeyCode::Enter));
    let (filtered, request) = take_project_request(&mut dashboard);
    assert_eq!(
        request,
        ProjectDiscoveryRequest::Directory {
            path: "/work".into(),
            filter: "app".into()
        }
    );
    dashboard.apply_project_discovery(&filtered, Err("Cannot read this folder".into()));
    activate_project_control(&mut dashboard, WizardControl::ProjectRetry);
    let (retry, retried_request) = take_project_request(&mut dashboard);
    assert_eq!(retried_request, request);
    assert_eq!(
        project_wizard(&dashboard).project_picker.folder_filter,
        "app"
    );
    dashboard.apply_project_discovery(
        &retry,
        Ok(ProjectDiscovery {
            entries: vec![discovered_repository("/work/app")],
            directory: Some("/work".into()),
            parent: Some("/".into()),
            truncated: false,
        }),
    );
    // A repository can be browsed for nested projects without selecting it.
    assert_eq!(
        activate_project_control(&mut dashboard, WizardControl::ProjectOpenFolder),
        DashboardAction::None
    );
    let (opened, request) = take_project_request(&mut dashboard);
    assert_eq!(
        request,
        ProjectDiscoveryRequest::Directory {
            path: "/work/app".into(),
            filter: String::new()
        }
    );
    assert!(
        project_wizard(&dashboard)
            .project_picker
            .folder_filter
            .is_empty()
    );
    assert!(!project_wizard(&dashboard).bundle_creation_in_flight);
    dashboard.apply_project_discovery(
        &opened,
        Ok(ProjectDiscovery {
            entries: vec![
                discovered_repository("/work/app"),
                discovered_repository("/work/app/nested"),
            ],
            directory: Some("/work/app".into()),
            parent: Some("/work".into()),
            truncated: false,
        }),
    );
    ready_key(&mut dashboard, key(KeyCode::Down));
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::CreateBundle {
            sources: vec!["/work/app/nested".into()]
        }
    );
}

fn project_picker_multiple_discovery_selection_toggles_members_and_can_change_primary() {
    let mut dashboard = dashboard_at_project_picker();
    focus_project_control(&mut dashboard, WizardControl::ProjectMultiple);
    ready_key(&mut dashboard, key(KeyCode::Char(' ')));
    activate_project_control(&mut dashboard, WizardControl::ProjectGithub);
    let (context, _) = take_project_request(&mut dashboard);
    dashboard.apply_project_discovery(
        &context,
        Ok(discovered_projects(vec![
            discovered_repository("owner/app"),
            discovered_repository("owner/shared"),
        ])),
    );
    assert_eq!(
        activate_project_control(&mut dashboard, WizardControl::ProjectResults),
        DashboardAction::None
    );
    ready_key(&mut dashboard, key(KeyCode::Down));
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert_eq!(
        project_wizard(&dashboard).new_bundle_repositories,
        ["owner/app", "owner/shared"]
    );
    // Choosing an already-selected result removes it, without launching.
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert_eq!(
        project_wizard(&dashboard).new_bundle_repositories,
        ["owner/app"]
    );
    captured_drawn(&mut dashboard, 100, 30);
    ready_key(&mut dashboard, key(KeyCode::Enter));
    activate_project_control(&mut dashboard, WizardControl::ProjectMakePrimary);
    captured_drawn(&mut dashboard, 100, 30);
    assert_eq!(
        activate_project_control(&mut dashboard, WizardControl::Next),
        DashboardAction::CreateBundle {
            sources: vec!["owner/shared".into(), "owner/app".into()]
        }
    );
}

#[test]
fn project_picker_rejects_late_success_and_failure_after_switching_tabs() {
    let mut dashboard = dashboard_at_project_picker();
    activate_project_control(&mut dashboard, WizardControl::ProjectGithub);
    let (github, _) = take_project_request(&mut dashboard);
    activate_project_control(&mut dashboard, WizardControl::ProjectFolders);
    let (folders, _) = take_project_request(&mut dashboard);
    dashboard.apply_project_discovery(
        &github,
        Ok(discovered_projects(vec![discovered_repository(
            "owner/stale",
        )])),
    );
    dashboard.apply_project_discovery(&github, Err("old GitHub error".into()));
    let picker = &project_wizard(&dashboard).project_picker;
    assert_eq!(picker.tab, ProjectTab::Folders);
    assert!(picker.loading);
    assert!(picker.entries.is_empty());
    assert!(picker.error.is_none());
    activate_project_control(&mut dashboard, WizardControl::ProjectUrl);
    type_source(&mut dashboard, "owner/draft");
    dashboard.apply_project_discovery(
        &folders,
        Ok(discovered_projects(vec![discovered_repository(
            "/stale/repo",
        )])),
    );
    let wizard = project_wizard(&dashboard);
    assert_eq!(wizard.project_picker.tab, ProjectTab::Url);
    assert!(wizard.project_picker.entries.is_empty());
    assert_eq!(wizard.new_bundle_source, "owner/draft");
    assert_eq!(
        wizard.form.borrow().focused(),
        Some(WizardControl::NewBundleSource)
    );
}

#[test]
fn project_picker_reopened_wizard_rejects_previous_discovery_context() {
    let mut dashboard = dashboard_at_project_picker();
    activate_project_control(&mut dashboard, WizardControl::ProjectGithub);
    let (old_context, _) = take_project_request(&mut dashboard);
    activate_project_control(&mut dashboard, WizardControl::Cancel);
    assert!(!matches!(dashboard.mode, Mode::New(_)));
    ready_open_new_wizard(&mut dashboard);
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    activate_project_control(&mut dashboard, WizardControl::ProjectGithub);
    let (context, _) = take_project_request(&mut dashboard);
    assert_ne!(context, old_context);
    dashboard.apply_project_discovery(
        &old_context,
        Ok(discovered_projects(vec![discovered_repository(
            "owner/stale",
        )])),
    );
    dashboard.apply_project_discovery(&old_context, Err("stale failure".into()));
    assert!(project_wizard(&dashboard).project_picker.loading);
    assert!(project_wizard(&dashboard).project_picker.entries.is_empty());
    assert!(project_wizard(&dashboard).project_picker.error.is_none());
    dashboard.apply_project_discovery(
        &context,
        Ok(discovered_projects(vec![discovered_repository(
            "owner/current",
        )])),
    );
    assert_eq!(
        project_wizard(&dashboard).project_picker.entries[0].source,
        "owner/current"
    );
}

#[test]
fn project_picker_retry_preserves_query_and_multiple_repository_selection() {
    let mut dashboard = dashboard_at_project_picker();
    activate_project_control(&mut dashboard, WizardControl::ProjectUrl);
    focus_project_control(&mut dashboard, WizardControl::ProjectMultiple);
    ready_key(&mut dashboard, key(KeyCode::Char(' ')));
    focus_project_control(&mut dashboard, WizardControl::NewBundleSource);
    type_source(&mut dashboard, "owner/main");
    ready_key(&mut dashboard, key(KeyCode::Enter));
    activate_project_control(&mut dashboard, WizardControl::ProjectGithub);
    take_project_request(&mut dashboard);
    type_source(&mut dashboard, "owner/shared");
    ready_key(&mut dashboard, key(KeyCode::Enter));
    let (failed, request) = take_project_request(&mut dashboard);
    dashboard.apply_project_discovery(&failed, Err("GitHub unavailable.".into()));
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let text = draw_project_picker(&mut dashboard, &mut terminal).join("\n");
    assert!(text.contains("GitHub unavailable"), "{text}");
    assert!(text.contains("owner/main"), "{text}");
    assert_eq!(
        click_project_text(&mut dashboard, &mut terminal, "Retry"),
        DashboardAction::None
    );
    let (retry, retried_request) = take_project_request(&mut dashboard);
    assert_eq!(retried_request, request);
    assert_ne!(retry, failed);
    assert_eq!(
        project_wizard(&dashboard).project_picker.query,
        "owner/shared"
    );
    assert_eq!(
        project_wizard(&dashboard).new_bundle_repositories,
        ["owner/main"]
    );
    dashboard.apply_project_discovery(
        &retry,
        Ok(discovered_projects(vec![discovered_repository(
            "owner/shared",
        )])),
    );
    assert_eq!(
        activate_project_control(&mut dashboard, WizardControl::ProjectResults),
        DashboardAction::None
    );
    assert_eq!(
        project_wizard(&dashboard).new_bundle_repositories,
        ["owner/main", "owner/shared"]
    );
    assert_eq!(
        activate_project_control(&mut dashboard, WizardControl::Next),
        DashboardAction::CreateBundle {
            sources: vec!["owner/main".into(), "owner/shared".into()]
        }
    );
}

fn project_picker_mouse_tabs_and_single_repository_choice_work_on_small_terminals() {
    for (width, height) in [(80, 24), (60, 20)] {
        let mut dashboard = dashboard_at_project_picker();
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        assert_eq!(
            click_project_text(&mut dashboard, &mut terminal, "GitHub"),
            DashboardAction::None
        );
        let (context, _) = take_project_request(&mut dashboard);
        dashboard.apply_project_discovery(
            &context,
            Ok(discovered_projects(vec![discovered_repository(
                "owner/visible",
            )])),
        );
        let lines = draw_project_picker(&mut dashboard, &mut terminal);
        let text = lines.join("\n");
        for label in [
            "Choose a project",
            "Folders",
            "Paste URL",
            "Search",
            "owner/visible",
            "Use project",
        ] {
            assert!(
                text.contains(label),
                "missing {label:?} at {width}x{height}:\n{text}"
            );
        }
        assert!(!text.contains("Add repository"), "{text}");
        assert_eq!(
            click_project_text(&mut dashboard, &mut terminal, "owner/visible"),
            DashboardAction::None
        );
        assert_eq!(
            project_wizard(&dashboard).form.borrow().focused(),
            Some(WizardControl::ProjectResults)
        );
        assert_eq!(
            ready_key(&mut dashboard, key(KeyCode::Enter)),
            DashboardAction::CreateBundle {
                sources: vec!["owner/visible".into()]
            }
        );
    }
}

fn project_picker_folder_choices_and_filter_remain_visible_on_small_terminals() {
    for (width, height) in [(80, 24), (60, 20)] {
        let mut dashboard = dashboard_at_project_picker();
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        click_project_text(&mut dashboard, &mut terminal, "Folders");
        let (context, _) = take_project_request(&mut dashboard);
        dashboard.apply_project_discovery(
            &context,
            Ok(ProjectDiscovery {
                entries: vec![discovered_repository("/work/visible")],
                directory: Some("/work".into()),
                parent: Some("/".into()),
                truncated: false,
            }),
        );
        let text = draw_project_picker(&mut dashboard, &mut terminal).join("\n");
        for label in ["/work/visible", "Filter:", "Use project"] {
            assert!(
                text.contains(label),
                "missing {label:?} at {width}x{height}:\n{text}"
            );
        }
        assert_eq!(
            click_project_text(&mut dashboard, &mut terminal, "Use project"),
            DashboardAction::CreateBundle {
                sources: vec!["/work/visible".into()],
            }
        );
    }
}

fn project_picker_results_remain_visible_with_multiple_selected_repositories() {
    for (width, height) in [(80, 24), (60, 20)] {
        let mut dashboard = dashboard_at_project_picker();
        focus_project_control(&mut dashboard, WizardControl::ProjectMultiple);
        ready_key(&mut dashboard, key(KeyCode::Char(' ')));
        activate_project_control(&mut dashboard, WizardControl::ProjectGithub);
        let (context, _) = take_project_request(&mut dashboard);
        dashboard.apply_project_discovery(
            &context,
            Ok(discovered_projects(vec![
                discovered_repository("owner/app"),
                discovered_repository("owner/shared"),
                discovered_repository("owner/docs"),
                discovered_repository("owner/next"),
            ])),
        );
        focus_project_control(&mut dashboard, WizardControl::ProjectResults);
        for _ in 0..3 {
            ready_key(&mut dashboard, key(KeyCode::Enter));
            ready_key(&mut dashboard, key(KeyCode::Down));
        }
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let text = draw_project_picker(&mut dashboard, &mut terminal).join("\n");
        assert!(
            text.contains("owner/next"),
            "selected search result disappears at {width}x{height}:\n{text}"
        );
        assert!(text.contains("Use 3 repositories"), "{text}");
        assert_eq!(
            ready_key(&mut dashboard, key(KeyCode::Enter)),
            DashboardAction::None
        );
        assert_eq!(
            activate_project_control(&mut dashboard, WizardControl::Next),
            DashboardAction::CreateBundle {
                sources: vec![
                    "owner/app".into(),
                    "owner/shared".into(),
                    "owner/docs".into(),
                    "owner/next".into()
                ],
            }
        );
    }
}

fn project_picker_mouse_multiple_toggle_and_url_add_keep_input_above_actions() {
    for (width, height) in [(80, 24), (60, 20)] {
        let mut dashboard = dashboard_at_project_picker();
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        click_project_text(&mut dashboard, &mut terminal, "Paste URL");
        click_project_text(
            &mut dashboard,
            &mut terminal,
            "Use several repositories together",
        );
        assert!(project_wizard(&dashboard).project_picker.multiple);
        focus_project_control(&mut dashboard, WizardControl::NewBundleSource);
        type_source(&mut dashboard, "owner/visible");
        let lines = draw_project_picker(&mut dashboard, &mut terminal);
        let text = lines.join("\n");
        let source_row = lines
            .iter()
            .position(|line| line.contains("owner/visible"))
            .unwrap_or_else(|| {
                panic!(
                    "source missing at {width}x{height}:\n{text}\n{:?}",
                    project_wizard(&dashboard).new_bundle_source
                )
            });
        let action_row = lines
            .iter()
            .position(|line| line.contains("Add repository"))
            .unwrap();
        assert!(source_row < action_row, "{width}x{height}:\n{text}");
        assert_eq!(
            click_project_text(&mut dashboard, &mut terminal, "Add repository"),
            DashboardAction::None
        );
        assert_eq!(
            project_wizard(&dashboard).new_bundle_repositories,
            ["owner/visible"]
        );
        assert!(project_wizard(&dashboard).new_bundle_source.is_empty());
        // The spaced form can exceed a short terminal. Tab must reveal the
        // repository actions rather than leave their controls off screen.
        for _ in 0..20 {
            if project_wizard(&dashboard).form.borrow().focused()
                == Some(WizardControl::NewBundleRemove)
            {
                break;
            }
            ready_key(&mut dashboard, key(KeyCode::Tab));
            draw_project_picker(&mut dashboard, &mut terminal);
        }
        assert_eq!(
            project_wizard(&dashboard).form.borrow().focused(),
            Some(WizardControl::NewBundleRemove)
        );
        let text = draw_project_picker(&mut dashboard, &mut terminal).join("\n");
        assert!(text.contains("Main  owner/visible"), "{text}");
        assert!(text.contains("Remove"), "{text}");
        assert_dialog_spacing(
            &buffer_lines(terminal.backend().buffer()),
            "Choose a project",
            "Use 1 repository",
        );
    }
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
            false,
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
            remember_container_size: false,
            subagents: Some(mj_core::subagent::SubagentPolicy::Native),
            review: None,
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
                legacy_subagents: Default::default(),
                guardian_review_model: None,
                codex_profile: None,
                kind,
                home: PathBuf::from("/profiles/harness"),
                environment: Default::default(),
            },
        )]);
        config
            .profiles
            .insert("profile-2".into(), config.profiles["profile"].clone());
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

    // OpenCode and MiMo Code prompt for permission but run commands on the
    // host without a sandbox, so a prompt is not a guardian.
    // Hard-won: fd92b0cc9: MiMo Code shipped without the raw-target warning although it has no sandbox.
    for kind in [
        HarnessKind::Kimi,
        HarnessKind::OpenCode,
        HarnessKind::Mimo,
        HarnessKind::Dsh,
    ] {
        let marked = profile_step(kind);
        assert!(
            marked.contains(mj_chat::theme::glyphs().warning),
            "{kind:?}: {marked}"
        );
        assert!(
            marked.contains("No guardian approval mode"),
            "{kind:?}: {marked}"
        );
        assert!(
            marked.contains("do not run on a raw, unsandboxed target"),
            "{kind:?}: {marked}"
        );
    }

    for kind in [
        HarnessKind::Codex,
        HarnessKind::Claude,
        HarnessKind::Grok,
        HarnessKind::Muse,
    ] {
        let quiet = profile_step(kind);
        assert!(
            !quiet.contains(mj_chat::theme::glyphs().warning),
            "{kind:?}: {quiet}"
        );
        assert!(
            !quiet.contains("No guardian approval mode"),
            "{kind:?}: {quiet}"
        );
    }
}

/// The profile step reads as a table: the marker, profile, harness, and quota
/// columns start at the same cell on the heading and on every row.
/// A profile whose installation is gone is not offered: the profile step
/// lists only what is there and names the missing one under the table, with
/// why and what to do.
fn a_missing_profile_is_left_out_of_the_profile_step_and_named() {
    let mut config = config();
    config.profiles.insert(
        "kimi-1".into(),
        HarnessProfile {
            enabled: true,
            context_window_bytes: None,
            legacy_subagents: Default::default(),
            guardian_review_model: None,
            codex_profile: None,
            kind: HarnessKind::Kimi,
            home: PathBuf::from("/profiles/kimi"),
            environment: Default::default(),
        },
    );
    let mut dashboard = DashboardState::new(config, State::default(), BTreeMap::new());
    ready_open_new_wizard(&mut dashboard);
    crate::test_support::answer_availability(&mut dashboard, |dashboard| {
        let mut answer = crate::test_support::availability_answer(dashboard, &[], &[]);
        answer.profiles.get_mut("kimi-1").unwrap().1 = Some(
            "Kimi Code is not installed here: /profiles/kimi does not exist and `kimi` is not \
             on PATH. Install Kimi Code, or turn the profile off in Settings › Profiles."
                .into(),
        );
        answer
    });
    let text = captured_drawn(&mut dashboard, 120, 30).join("\n");
    assert!(text.contains("claude-1"), "{text}");
    assert!(
        text.contains("kimi-1: Kimi Code is not installed here"),
        "{text}"
    );
    assert!(
        dashboard.offered_profiles().all(|(id, _)| id != "kimi-1"),
        "a missing profile is not offered"
    );
}

fn new_session_profile_step_aligns_its_columns() {
    let mut config = config();
    config.profiles.insert(
        "kimi-1".into(),
        HarnessProfile {
            enabled: true,
            context_window_bytes: None,
            legacy_subagents: Default::default(),
            guardian_review_model: None,
            codex_profile: None,
            kind: HarnessKind::Kimi,
            home: PathBuf::from("/profiles/kimi"),
            environment: Default::default(),
        },
    );
    let mut dashboard = DashboardState::new(config, State::default(), BTreeMap::new());
    ready_open_new_wizard(&mut dashboard);
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .unwrap();
    let lines = buffer_lines(terminal.backend().buffer());
    capture_render(120, 30, &lines);
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
    assert!(!claude.contains(mj_chat::theme::glyphs().warning));
    assert!(kimi.contains(mj_chat::theme::glyphs().warning));
    assert_eq!(
        cell_column(kimi, mj_chat::theme::glyphs().warning)
            + 1
            + u16::try_from(COLUMN_GAP).unwrap(),
        profile_column
    );
}

/// The profile step reports quota as two percentages remaining, one per
/// window, in their own columns and with no reset countdown.
fn new_session_profile_step_shows_weekly_and_five_hour_percentages() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.quotas = BTreeMap::from([(
        "claude-1".to_string(),
        ProfileQuota {
            banked_resets: None,
            profile_id: "claude-1".into(),
            harness: HarnessKind::Claude,
            windows: vec![
                QuotaWindow {
                    label: "Week".into(),
                    remaining_percent: Some(63),
                    duration_seconds: None,
                    model_scope: None,
                    used: None,
                    limit: None,
                    resets: Some("09:00 Aug 20".into()),
                    resets_at_epoch_seconds: Some(604_800),
                },
                QuotaWindow {
                    label: "5H".into(),
                    remaining_percent: Some(40),
                    duration_seconds: None,
                    model_scope: None,
                    used: None,
                    limit: None,
                    resets: Some("14:00 Aug 13".into()),
                    resets_at_epoch_seconds: Some(14_400),
                },
            ],
            extra: None,
            error: None,
            refreshed_at_epoch_seconds: 0,
            rate_limited_until_epoch_seconds: None,
        },
    )]);
    dashboard.begin_new();
    let lines = captured_drawn(&mut dashboard, 120, 30);

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
            legacy_subagents: Default::default(),
            guardian_review_model: None,
            codex_profile: None,
            kind: HarnessKind::Kimi,
            home: PathBuf::from("/profiles/kimi"),
            environment: Default::default(),
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

    // Both single-choice selectors were skipped when the wizard opened.
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
            false,
        )),
    );
    assert_eq!(
        dashboard.take_prerequisite_check().unwrap(),
        DashboardAction::CreateSession {
            remember_container_size: false,
            subagents: Some(mj_core::subagent::SubagentPolicy::Native),
            review: None,
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

fn directory_completion_lists_every_candidate_and_selects_with_keys() {
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
    dashboard.apply_path_completions(
        &dashboard.path_input_context(),
        "/opt/",
        PathCompletion {
            candidates: candidates.clone(),
            insert: None,
            truncated: false,
        },
    );

    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected directory editor");
    };
    // The popup scrolls, so every candidate the host sent is kept.
    assert_eq!(wizard.mounts.source.completions().len(), candidates.len());
    let rendered = captured_drawn(&mut dashboard, 100, 30).join("\n");
    assert!(rendered.contains(" matches "), "{rendered}");
    assert!(rendered.contains("/opt/directory-0/"), "{rendered}");

    ready_key(&mut dashboard, key(KeyCode::Down));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected directory editor");
    };
    assert_eq!(wizard.mounts.source, "/opt/directory-1/");
    assert!(!wizard.mounts.source.is_completing());

    let rendered = captured_drawn(&mut dashboard, 100, 30).join("\n");
    assert!(rendered.contains("Add directory"));
    assert!(rendered.contains("Cancel"));
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
        destination_checks: Default::default(),
        workspace: None,
        in_place: false,
        stops_subagents: true,
        source_unavailable: false,
        conversion: None,
        selection: mj_core::state::MoveSelection {
            subagents: None,
            workspace: Default::default(),
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

/// A Move stops the parent's sub-agents as a suspend does. The review counts
/// only the ones still at their task; idle ones have handed back and are not
/// mentioned. An in-place Move that keeps its children says nothing.
// Hard-won: 63ed79ef: Move review counted idle children as needing suspension
#[test]
fn move_review_counts_only_working_subagents() {
    let review_with = |working: bool, stops_subagents: bool| {
        let (mut dashboard, _) = dashboard_with_one_subagent();
        if working {
            dashboard
                .session_details
                .get_mut("child-session")
                .unwrap()
                .current_turn_started_at = Some(1);
        }
        let request_id = open_move_review(&mut dashboard);
        let mut preparation = move_preparation();
        preparation.in_place = !stops_subagents;
        preparation.stops_subagents = stops_subagents;
        assert!(dashboard.apply_move_preparation(request_id, preparation));
        let mut terminal = Terminal::new(TestBackend::new(200, 44)).expect("terminal");
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .expect("draw move review");
        let rendered = buffer_lines(terminal.backend().buffer()).join(" ");
        rendered.split_whitespace().collect::<Vec<_>>().join(" ")
    };
    let idle = review_with(false, true);
    assert!(!idle.contains("will be stopped"), "{idle}");
    let working = review_with(true, true);
    assert!(
        working.contains(
            "1 working sub-agent will be stopped; the session is told which when it resumes."
        ),
        "{working}"
    );
    let kept = review_with(true, false);
    assert!(!kept.contains("will be stopped"), "{kept}");
    assert!(
        kept.contains("Only the harness and profile are replaced"),
        "{kept}"
    );
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

// Hard-won: 16e952f7: same-harness resumes incorrectly displayed a Lossy warning
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
    // I1-6: the session's own profile is selected, and a same-harness resume
    // continues the native conversation, so it carries no loss warning.
    assert!(rendered.contains("Same agent: the conversation continues natively."));
    assert!(!rendered.contains("Lossy: text only"));

    let Mode::Resume(wizard) = &dashboard.mode else {
        panic!("resume wizard is open");
    };
    let initial = wizard.profile;
    let session_harness = dashboard.state.sessions[&wizard.session_id].harness_kind;
    let lossy = dashboard
        .resume_wizard_profiles(wizard)
        .iter()
        .position(|(_, harness)| *harness != session_harness)
        .expect("the fixture offers a cross-harness profile");
    let Mode::Resume(wizard) = &mut dashboard.mode else {
        unreachable!()
    };
    wizard.profile = lossy;
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw cross-harness selection");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("Lossy: text only; tool calls + reasoning dropped."));
    let Mode::Resume(wizard) = &mut dashboard.mode else {
        unreachable!()
    };
    wizard.profile = initial;

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

/// Opens the restore wizard for `wiki_id`, walks it to the review, and
/// presses its Restore button.
fn press_archive_restore(dashboard: &mut DashboardState, wiki_id: &str) -> DashboardAction {
    assert_eq!(
        dashboard.begin_archive_restore(wiki_id.into(), "Pomegranate work".into(), None, None),
        DashboardAction::None
    );
    ready_key(dashboard, key(KeyCode::Enter));
    ready_key(dashboard, key(KeyCode::Enter));
    let Mode::Resume(wizard) = &dashboard.mode else {
        panic!("expected the restore wizard, got {:?}", dashboard.mode);
    };
    assert_eq!(wizard.step, WizardStep::Review);
    ready_key(dashboard, key(KeyCode::Enter))
}

fn restores(action: &DashboardAction, expected: &str) -> bool {
    matches!(action, DashboardAction::RestoreArchivedSession { wiki_id, .. } if wiki_id == expected)
}

/// Restore on a restore's review closes the wizard, as a live resume does
/// once its launch starts. The restore used to leave the review open, and a
/// second press restored the archive a second time.
// Hard-won: 9332aae3: successful restore left its review open and allowed a second restore
#[test]
fn restoring_an_archive_closes_the_wizard_when_restore_is_pressed() {
    let mut dashboard = dashboard_with_session(stopped_session());
    let action = press_archive_restore(&mut dashboard, "wiki-1");
    assert!(restores(&action, "wiki-1"), "{action:?}");
    assert!(
        matches!(dashboard.mode, Mode::Dashboard),
        "{:?}",
        dashboard.mode
    );

    let second = ready_key(&mut dashboard, key(KeyCode::Enter));
    assert!(
        !matches!(second, DashboardAction::RestoreArchivedSession { .. }),
        "{second:?}"
    );
}

/// Until a restore reports back, its archive cannot be restored again.
/// Opening it is refused with a notice; other archives are not held up.
// Hard-won: 9332aae3: an archive could be restored again while its first restore was pending
#[test]
fn an_archive_cannot_be_restored_again_until_its_restore_reports_back() {
    let mut dashboard = dashboard_with_session(stopped_session());
    let first = press_archive_restore(&mut dashboard, "wiki-1");
    assert!(restores(&first, "wiki-1"), "{first:?}");

    assert_eq!(
        dashboard.begin_archive_restore("wiki-1".into(), "Pomegranate work".into(), None, None),
        DashboardAction::None
    );
    assert!(
        matches!(dashboard.mode, Mode::Dashboard),
        "{:?}",
        dashboard.mode
    );
    assert_eq!(
        dashboard.notice().as_deref(),
        Some("Restore already started.")
    );

    let other = press_archive_restore(&mut dashboard, "wiki-2");
    assert!(restores(&other, "wiki-2"), "{other:?}");

    // The dashboard loop ends the restore when the daemon registers the
    // new session. After that the archive can be restored again.
    dashboard.finish_archive_restore(&first);
    let again = press_archive_restore(&mut dashboard, "wiki-1");
    assert!(restores(&again, "wiki-1"), "{again:?}");
}

/// The review refuses Restore for an archive whose restore has already
/// started, whatever started it, and keeps the draft on screen.
// Hard-won: 9332aae3: pressing Restore during an in-flight restore was accepted
#[test]
fn pressing_restore_after_the_restore_started_returns_no_action() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_archive_restore("wiki-1".into(), "Pomegranate work".into(), None, None);
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    assert!(dashboard.claim_archive_restore("wiki-1"));

    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert!(
        matches!(&dashboard.mode, Mode::Resume(wizard) if wizard.step == WizardStep::Review),
        "{:?}",
        dashboard.mode
    );
    assert_eq!(
        dashboard.notice().as_deref(),
        Some("Restore already started.")
    );
}

/// A failed restore shows the launch-failure dialog over the dashboard, not
/// over a restore review that could be pressed again. Its Retry sends the
/// same restore once, and that retry holds the archive until it reports back.
// Hard-won: 9332aae3: restore failures were silent and could not be retried
#[test]
fn a_failed_restore_reports_the_failure_and_retries_it_once() {
    let mut dashboard = dashboard_with_session(stopped_session());
    let restore = press_archive_restore(&mut dashboard, "wiki-1");
    assert!(restores(&restore, "wiki-1"), "{restore:?}");

    // What the dashboard loop does with the restore's failure report.
    dashboard.finish_archive_restore(&restore);
    dashboard.show_launch_failure("wiki restore failed", Some(restore.clone()));
    let mut terminal = Terminal::new(TestBackend::new(90, 25)).expect("terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw launch failure");
    let text = buffer_lines(terminal.backend().buffer()).join("\n");
    assert!(text.contains("Launch failed"), "{text}");
    assert!(text.contains("wiki restore failed"), "{text}");

    dashboard.handle_key(key(KeyCode::Right));
    assert_eq!(dashboard.handle_key(key(KeyCode::Enter)), restore);
    assert!(
        matches!(dashboard.mode, Mode::Dashboard),
        "{:?}",
        dashboard.mode
    );
    assert_eq!(
        dashboard.begin_archive_restore("wiki-1".into(), "Pomegranate work".into(), None, None),
        DashboardAction::None
    );
    assert!(
        matches!(dashboard.mode, Mode::Dashboard),
        "{:?}",
        dashboard.mode
    );

    dashboard.finish_archive_restore(&restore);
    let again = press_archive_restore(&mut dashboard, "wiki-1");
    assert!(restores(&again, "wiki-1"), "{again:?}");
}

fn cache_mount() -> AdditionalMount {
    AdditionalMount {
        source: "/opt/cache".into(),
        destination: "/mnt/cache".into(),
        access: MountAccess::Cow,
    }
}

/// Walks the resume wizard for the stopped session to its review and presses
/// Resume.
fn press_resume(dashboard: &mut DashboardState) -> DashboardAction {
    open_resume_wizard(dashboard);
    ready_key(dashboard, key(KeyCode::Enter));
    ready_key(dashboard, key(KeyCode::Enter));
    assert_eq!(resume_wizard(dashboard).step, WizardStep::Review);
    ready_key(dashboard, key(KeyCode::Enter))
}

/// The resume launch inside the check that Resume sends.
fn resume_launch(check: &DashboardAction) -> DashboardAction {
    match check {
        DashboardAction::ValidateSessionMounts { launch, .. } => resume_launch(launch),
        DashboardAction::PreflightResumeRepositories { launch } => {
            assert!(
                matches!(launch.as_ref(), DashboardAction::ResumeSession { .. }),
                "{launch:?}"
            );
            launch.as_ref().clone()
        }
        other => panic!("expected the resume check, got {other:?}"),
    }
}

fn resume_receipt() -> mj_core::state::ResumeRepositorySourceReceipt {
    mj_core::state::ResumeRepositorySourceReceipt {
        session_id: "session-1".into(),
        bundle_id: "hel".into(),
        checkpoint_sha256: "a".repeat(64),
        repositories: Vec::new(),
    }
}

/// What the dashboard loop does with a result of the resume check
/// (`SessionMountValidation` and `ResumeRepositoryPreflight` in
/// mj-cli/src/dashboard/io.rs): it applies the result while the generation
/// the check was sent under is current, and drops it otherwise. Answers
/// whether the result was applied.
fn deliver_resume_check_result(
    dashboard: &mut DashboardState,
    generation: u64,
    apply: impl FnOnce(&mut DashboardState),
) -> bool {
    if generation != dashboard.session_preflight_generation() {
        return false;
    }
    apply(dashboard);
    true
}

/// Resume on the review stays open while the check runs. A second press
/// used to send a second check beside the first, with or without attached
/// directories to check first.
// Hard-won: 50b85cae: Resume could send duplicate checks
#[test]
fn pressing_resume_twice_sends_one_check() {
    for mounts in [Vec::new(), vec![cache_mount()]] {
        let with_mounts = !mounts.is_empty();
        let mut session = stopped_session();
        session.additional_mounts = mounts;
        let mut dashboard = dashboard_with_session(session);

        let first = press_resume(&mut dashboard);
        if with_mounts {
            assert!(
                matches!(first, DashboardAction::ValidateSessionMounts { .. }),
                "{first:?}"
            );
        } else {
            assert!(
                matches!(first, DashboardAction::PreflightResumeRepositories { .. }),
                "{first:?}"
            );
        }

        assert_eq!(
            ready_key(&mut dashboard, key(KeyCode::Enter)),
            DashboardAction::None,
            "with mounts: {with_mounts}"
        );
        assert_eq!(
            dashboard.notice().as_deref(),
            Some("Resume already started.")
        );
        assert_eq!(resume_wizard(&dashboard).step, WizardStep::Review);
    }
}

/// Once the check has reported back, another result sent under the same
/// generation is dropped. A late Ready used to close whatever the first
/// result had opened and start the launch anyway.
// Hard-won: 50b85cae: late Ready could close the wrong Resume dialog and launch it
#[test]
fn a_late_second_ready_is_dropped() {
    #[derive(Debug, Clone, Copy)]
    enum First {
        Ready,
        RawConversion,
        MovedOrigin,
        AttachmentFailure,
    }
    for first in [
        First::Ready,
        First::RawConversion,
        First::MovedOrigin,
        First::AttachmentFailure,
    ] {
        let mut session = stopped_session();
        if matches!(first, First::AttachmentFailure) {
            session.additional_mounts = vec![cache_mount()];
        }
        let mut dashboard = dashboard_with_session(session);
        let launch = resume_launch(&press_resume(&mut dashboard));
        // The dashboard loop sends the check under the generation current
        // when it receives the action.
        let generation = dashboard.session_preflight_generation();

        let applied =
            deliver_resume_check_result(&mut dashboard, generation, |dashboard| match first {
                First::Ready => dashboard.finish_resume_repository_preflight(),
                First::RawConversion => dashboard.show_raw_conversion_confirmation(
                    launch.clone(),
                    resume_receipt(),
                    raw_conversion_preview(),
                ),
                First::MovedOrigin => dashboard.show_repository_origin_dialog(
                    "session-1".into(),
                    "hel".into(),
                    "b".repeat(40),
                    "https://github.com/example/old.git".into(),
                    "https://github.com/example/new.git".into(),
                    launch.clone(),
                ),
                First::AttachmentFailure => dashboard.apply_session_mount_preflight_failure(
                    "/opt/cache",
                    "source path /opt/cache does not exist or is not a directory".into(),
                ),
            });
        assert!(applied, "{first:?}");
        let after_first = dashboard.mode.clone();

        let late = deliver_resume_check_result(&mut dashboard, generation, |dashboard| {
            dashboard.finish_resume_repository_preflight()
        });
        assert!(!late, "{first:?}: the late Ready was applied");
        assert_eq!(dashboard.mode, after_first, "{first:?}");
    }
}

/// A second raw-conversion result for the same check is dropped instead of
/// opening a second confirmation over the first. Cancel on the one
/// confirmation returns to the wizard, where Resume starts a new check.
// Hard-won: 50b85cae: late raw conversion could stack another confirmation
#[test]
fn a_late_second_raw_conversion_result_does_not_stack_a_dialog() {
    let mut dashboard = dashboard_with_session(stopped_session());
    let launch = resume_launch(&press_resume(&mut dashboard));
    let generation = dashboard.session_preflight_generation();
    let show = |dashboard: &mut DashboardState| {
        dashboard.show_raw_conversion_confirmation(
            launch.clone(),
            resume_receipt(),
            raw_conversion_preview(),
        )
    };

    assert!(deliver_resume_check_result(
        &mut dashboard,
        generation,
        show
    ));
    assert!(!deliver_resume_check_result(
        &mut dashboard,
        generation,
        show
    ));
    let Mode::Confirm(dialog) = &dashboard.mode else {
        panic!(
            "expected the conversion confirmation, got {:?}",
            dashboard.mode
        );
    };
    let crate::dialogs::Confirmation::ConvertRawCheckout { previous, .. } = &dialog.confirmation
    else {
        panic!("expected the conversion confirmation, got {dialog:?}");
    };
    assert!(
        matches!(previous.as_ref(), Mode::Resume(_)),
        "the confirmation sits over another dialog: {previous:?}"
    );

    // Cancel is focused first.
    assert_eq!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::None
    );
    assert_eq!(resume_wizard(&dashboard).step, WizardStep::Review);
    let again = ready_key(&mut dashboard, key(KeyCode::Enter));
    assert!(
        matches!(again, DashboardAction::PreflightResumeRepositories { .. }),
        "{again:?}"
    );
}

/// A failed check ends the hold, so Resume on the same review sends a new
/// check. Closing the wizard ends it too: its check's result is dropped and
/// a reopened wizard's Resume is not refused.
#[test]
fn resume_starts_a_new_check_once_the_first_fails_or_the_wizard_closes() {
    let mut dashboard = dashboard_with_session(stopped_session());
    press_resume(&mut dashboard);
    let generation = dashboard.session_preflight_generation();
    // What the dashboard loop does when the repository check fails.
    assert!(deliver_resume_check_result(
        &mut dashboard,
        generation,
        |dashboard| {
            dashboard.set_notice("Could not check checkpoint repositories: offline");
            dashboard.end_resume_preflight();
        }
    ));
    assert_eq!(resume_wizard(&dashboard).step, WizardStep::Review);
    let retry = ready_key(&mut dashboard, key(KeyCode::Enter));
    assert!(
        matches!(retry, DashboardAction::PreflightResumeRepositories { .. }),
        "{retry:?}"
    );

    let generation = dashboard.session_preflight_generation();
    ready_key(&mut dashboard, key(KeyCode::Esc));
    assert!(
        matches!(dashboard.mode, Mode::Dashboard),
        "{:?}",
        dashboard.mode
    );
    assert!(!deliver_resume_check_result(
        &mut dashboard,
        generation,
        |dashboard| dashboard.finish_resume_repository_preflight()
    ));
    let reopened = press_resume(&mut dashboard);
    assert!(
        matches!(
            reopened,
            DashboardAction::PreflightResumeRepositories { .. }
        ),
        "{reopened:?}"
    );
}

fn resume_profile_step_aligns_its_columns_and_explains_the_marker() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.config.profiles.insert(
        "kimi-1".into(),
        HarnessProfile {
            enabled: true,
            context_window_bytes: None,
            legacy_subagents: Default::default(),
            guardian_review_model: None,
            codex_profile: None,
            kind: HarnessKind::Kimi,
            home: PathBuf::from("/profiles/kimi"),
            environment: Default::default(),
        },
    );
    open_resume_wizard(&mut dashboard);
    let mut terminal = Terminal::new(TestBackend::new(120, 24)).expect("terminal");
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .expect("draw resume profile step");
    let lines = buffer_lines(terminal.backend().buffer());
    capture_render(120, 24, &lines);
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
    assert!(row("kimi-1").contains(mj_chat::theme::glyphs().warning));
    assert!(!row("claude-1").contains(mj_chat::theme::glyphs().warning));
    assert!(!row("codex-1").contains(mj_chat::theme::glyphs().warning));
    assert!(
        lines
            .iter()
            .any(|line| line.contains("No guardian approval mode; do not run on a raw")),
        "{lines:?}"
    );
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
    // fallback focus, past the delegation choice a Codex destination offers.
    // Returning to the target picker invalidates the request.
    for _ in 0..8 {
        if resume_wizard(&dashboard).form.borrow().focused() == Some(WizardControl::Back) {
            break;
        }
        ready_key(&mut dashboard, key(KeyCode::Tab));
    }
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
fn new_session_defaults_to_the_latest_size_on_its_host_and_clamps_to_capacity() {
    let gib = 1024 * 1024 * 1024;
    let mut state = State::default();
    state.container_sizes.insert(
        "local".into(),
        HostContainerSize {
            cpus: 24,
            memory_bytes: 36 * gib,
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
        local_storage_paths: Vec::new(),
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
            storage: Vec::new(),
        })),
        0,
    );
    set_local_container_capacity(
        &mut dashboard,
        HostContainerSize {
            cpus: 12,
            memory_bytes: 48 * gib,
        },
        HostContainerSize {
            cpus: 0,
            memory_bytes: 0,
        },
        HostContainerSize {
            cpus: 12,
            memory_bytes: 48 * gib * 4 / 5,
        },
        true,
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
            memory_bytes: 36 * gib,
        })
    );
}

#[test]
fn new_container_size_checkbox_is_explicit_and_reaches_the_create_action() {
    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
    dashboard.apply_deployment_capacity(
        "local",
        Ok(Some(mj_core::targets::DeploymentCapacityUsage {
            cpu_percent: None,
            memory_used_bytes: 0,
            memory_total_bytes: 64 << 30,
            logical_cores: 24,
            disk_total_bytes: None,
            storage: Vec::new(),
        })),
        0,
    );
    set_local_container_capacity(
        &mut dashboard,
        HostContainerSize {
            cpus: 24,
            memory_bytes: 64 << 30,
        },
        HostContainerSize {
            cpus: 0,
            memory_bytes: 0,
        },
        HostContainerSize {
            cpus: 24,
            memory_bytes: (64 << 30) * 4 / 5,
        },
        true,
    );
    ready_open_new_wizard(&mut dashboard);
    ready_key(&mut dashboard, key(KeyCode::Enter));
    assert_eq!(
        new_wizard(&dashboard).resource_allocation,
        Some(SessionResourceAllocation::Container {
            cpus: 4,
            memory_bytes: 8 << 30,
        })
    );
    assert!(!new_wizard(&dashboard).remember_container_size);
    let screen = captured_drawn(&mut dashboard, 100, 30).join("\n");
    assert!(screen.contains("Save as default sizing"), "{screen}");
    assert!(screen.contains("RAM left"), "{screen}");

    ready_key(&mut dashboard, key(KeyCode::Tab));
    ready_key(&mut dashboard, key(KeyCode::Tab));
    ready_key(&mut dashboard, key(KeyCode::Tab));
    assert_eq!(
        new_wizard(&dashboard).form.borrow().focused(),
        Some(WizardControl::RememberContainerSize)
    );
    ready_key(&mut dashboard, key(KeyCode::Char(' ')));
    assert!(new_wizard(&dashboard).remember_container_size);

    let action = dashboard.create_session_action_without_closing(new_wizard(&dashboard));
    assert!(matches!(
        action,
        DashboardAction::CreateSession {
            remember_container_size: true,
            ..
        }
    ));
}

#[test]
fn unknown_live_container_capacity_is_not_reported_as_zero() {
    let mut session = running_session();
    session.target = Some(mj_core::state::TargetLocator::LocalPodman {
        container_id: "unknown-size".into(),
        workspace_storage: Default::default(),
        borrowed_from: None,
    });
    session.resource_allocation = None;
    let mut state = State::default();
    state.sessions.insert(session.id.clone(), session);
    let mut dashboard = DashboardState::new(config(), state, BTreeMap::new());
    dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
    dashboard.apply_deployment_capacity(
        "local",
        Ok(Some(mj_core::targets::DeploymentCapacityUsage {
            cpu_percent: None,
            memory_used_bytes: 0,
            memory_total_bytes: 64 << 30,
            logical_cores: 24,
            disk_total_bytes: None,
            storage: Vec::new(),
        })),
        0,
    );
    set_local_container_capacity(
        &mut dashboard,
        HostContainerSize {
            cpus: 24,
            memory_bytes: 64 << 30,
        },
        HostContainerSize {
            cpus: 24,
            memory_bytes: (64 << 30) * 4 / 5,
        },
        HostContainerSize {
            cpus: 0,
            memory_bytes: 0,
        },
        false,
    );

    ready_open_new_wizard(&mut dashboard);
    ready_key(&mut dashboard, key(KeyCode::Enter));

    let wizard = new_wizard(&dashboard);
    assert!(wizard.resource_allocation.is_none());
    let error = wizard.sizing_error.as_deref().unwrap();
    assert!(error.contains("no recorded sizing"), "{error}");
    assert!(!error.contains("0 CPUs"), "{error}");
    let screen = captured_drawn(&mut dashboard, 100, 30).join("\n");
    assert!(screen.contains("RAM left unavailable"), "{screen}");
    assert!(!screen.contains("RAM left: 0"), "{screen}");
}

// Hard-won: d27eb9f56: the TUI ignored daemon reservations while sizing a new session.
#[test]
fn terminal_headroom_comes_from_the_daemons_reservation_adjusted_capacity() {
    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
    dashboard.apply_deployment_capacity(
        "local",
        Ok(Some(mj_core::targets::DeploymentCapacityUsage {
            cpu_percent: Some(20),
            memory_used_bytes: 16 << 30,
            memory_total_bytes: 64 << 30,
            logical_cores: 10,
            disk_total_bytes: None,
            storage: Vec::new(),
        })),
        0,
    );
    set_local_container_capacity(
        &mut dashboard,
        HostContainerSize {
            cpus: 10,
            memory_bytes: 64 << 30,
        },
        HostContainerSize {
            cpus: 8,
            memory_bytes: 16 << 30,
        },
        HostContainerSize {
            cpus: 2,
            memory_bytes: (64 << 30) * 4 / 5 - (16 << 30),
        },
        true,
    );

    let capacity = dashboard.host_container_capacity("podman").unwrap();
    assert_eq!(capacity.remaining.cpus, 2);
    ready_open_new_wizard(&mut dashboard);
    ready_key(&mut dashboard, key(KeyCode::Enter));
    let screen = captured_drawn(&mut dashboard, 100, 30).join("\n");
    assert!(screen.contains("2 CPUs"), "{screen}");
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
fn resume_keeps_a_legacy_allocation_below_the_new_session_minimum() {
    let mut session = stopped_session();
    session.resource_allocation = Some(SessionResourceAllocation::Container {
        cpus: 1,
        memory_bytes: 2 << 30,
    });
    let mut state = State::default();
    state.sessions.insert(session.id.clone(), session);
    let mut dashboard = DashboardState::new(config(), state, BTreeMap::new());

    dashboard.begin_resume_for("session-1");
    ready_key(&mut dashboard, key(KeyCode::Enter));

    let Mode::Resume(wizard) = &dashboard.mode else {
        panic!("expected resume wizard on target step");
    };
    assert_eq!(
        wizard.resource_allocation,
        Some(SessionResourceAllocation::Container {
            cpus: 1,
            memory_bytes: 2 << 30,
        })
    );
    assert_eq!(wizard.sizing_error, None);
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
fn revisiting_or_reselecting_a_target_preserves_edited_resources() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.begin_new();
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Tab));
    replace_resource_text(&mut dashboard, "4");
    ready_key(&mut dashboard, key(KeyCode::BackTab));
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
            false,
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
            false,
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
            false,
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
            false,
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

/// Opens the full new-session wizard through its live binding, for wizard
/// tests concerned with other behavior. No availability answer is needed:
/// until one arrives, everything configured is offered.
fn ready_open_new_wizard(dashboard: &mut DashboardState) -> DashboardAction {
    open_new_session_wizard(dashboard)
}

fn ready_key(dashboard: &mut DashboardState, event: crossterm::event::KeyEvent) -> DashboardAction {
    let action = dashboard.handle_key(event);
    capture_action(&action);
    action
}

/// An answer that a runtime is gone removes it from the open picker, and an
/// answer that it is back returns it, with nothing shown in between. A
/// check is never repeated while one runs, and an answer that arrives after
/// a newer check started is dropped.
#[test]
fn availability_answers_change_the_open_picker_only_when_they_differ() {
    let mut config = standard_local_targets_config();
    config.default_targets.clear();
    let mut dashboard = DashboardState::new(config, State::default(), BTreeMap::new());
    let Some(DashboardAction::CheckAvailability {
        generation: startup,
    }) = dashboard.take_availability_check()
    else {
        panic!("startup checks what is there");
    };
    open_new_session_wizard(&mut dashboard);
    assert_eq!(
        dashboard.take_availability_check(),
        None,
        "a check in flight is not repeated"
    );
    dashboard.handle_key(key(KeyCode::Enter));
    let before = crate::test_support::drawn(&mut dashboard, 160, 40).join("\n");
    assert!(before.contains("docker"), "{before}");
    assert!(!before.contains("checking"), "{before}");

    let answer = crate::test_support::availability_answer(&dashboard, &["docker"], &[]);
    dashboard.apply_availability_check(startup, Ok(answer));
    let gone = crate::test_support::drawn(&mut dashboard, 160, 40).join("\n");
    assert!(
        gone.contains("docker: docker is not installed here."),
        "a configured runtime that is gone is named: {gone}"
    );
    assert!(!gone.contains("unavailable"), "{gone}");

    // The picker opened while the startup check ran, so another is due.
    let Some(DashboardAction::CheckAvailability { generation: stale }) =
        dashboard.take_availability_check()
    else {
        panic!("opening the picker asked for a check");
    };
    dashboard.handle_key(key(KeyCode::Esc));
    open_new_session_wizard(&mut dashboard);
    let answer = crate::test_support::availability_answer(&dashboard, &[], &[]);
    dashboard.apply_availability_check(stale, Ok(answer));
    let back = crate::test_support::drawn(&mut dashboard, 160, 40).join("\n");
    assert!(!back.contains("not installed"), "{back}");
    let answer = crate::test_support::availability_answer(&dashboard, &["docker"], &[]);
    dashboard.apply_availability_check(stale, Ok(answer));
    assert!(
        dashboard.target_offered("docker"),
        "an answer to a finished check changes nothing"
    );
}

// Hard-won: #1370: an answer under the help overlay left a stale profile index, one mid-launch closed the wizard, and the startup check raced first-run setup.
#[test]
fn availability_answers_reach_a_wizard_under_help_and_spare_a_launch() {
    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    dashboard.hold_availability_check();
    assert_eq!(
        dashboard.take_availability_check(),
        None,
        "nothing is checked while first-run setup decides"
    );
    dashboard.release_availability_check();
    let Some(DashboardAction::CheckAvailability { generation }) =
        dashboard.take_availability_check()
    else {
        panic!("the startup check starts once setup is done");
    };
    open_new_session_wizard(&mut dashboard);
    let last = dashboard.offered_profiles().count() - 1;
    let chosen = dashboard.nth_offered_profile(last).unwrap();
    match &mut dashboard.mode {
        Mode::New(wizard) => wizard.profile = last,
        _ => panic!("expected the new-session wizard"),
    }
    dashboard.begin_help();
    let first = dashboard.nth_offered_profile(0).unwrap();
    let mut answer = crate::test_support::availability_answer(&dashboard, &[], &[]);
    answer.profiles.get_mut(&first).unwrap().1 = Some("gone".into());
    dashboard.apply_availability_check(generation, Ok(answer));
    dashboard.handle_key(key(KeyCode::Esc));
    let wizard = new_wizard(&dashboard);
    assert_eq!(
        dashboard.nth_offered_profile(wizard.profile).as_deref(),
        Some(chosen.as_str()),
        "the wizard under help keeps its choice"
    );
    crate::test_support::drawn(&mut dashboard, 140, 40);

    match &mut dashboard.mode {
        Mode::New(wizard) => wizard.step = WizardStep::Launching,
        _ => panic!("expected the new-session wizard"),
    }
    crate::test_support::answer_availability(&mut dashboard, |dashboard| {
        let mut answer = crate::test_support::availability_answer(dashboard, &[], &[]);
        for (_, missing) in answer.profiles.values_mut() {
            *missing = Some("gone".into());
        }
        answer
    });
    assert_eq!(
        new_wizard(&dashboard).step,
        WizardStep::Launching,
        "a submitted launch is left to its own preflight"
    );
    crate::test_support::drawn(&mut dashboard, 140, 40);
}

/// A runtime the wizard already chose that turns out to be gone sends the
/// wizard back to the target step with the reason, rather than launching on
/// it or silently swapping in another.
#[test]
fn a_chosen_runtime_that_disappears_returns_the_wizard_to_its_step() {
    let mut config = standard_local_targets_config();
    config.default_targets.clear();
    let mut dashboard = DashboardState::new(config, State::default(), BTreeMap::new());
    let Some(DashboardAction::CheckAvailability { generation }) =
        dashboard.take_availability_check()
    else {
        panic!("startup checks what is there");
    };
    open_new_session_wizard(&mut dashboard);
    let docker = dashboard
        .config
        .targets
        .keys()
        .position(|id| id == "docker")
        .unwrap();
    match &mut dashboard.mode {
        Mode::New(wizard) => {
            wizard.target = docker;
            wizard.step = WizardStep::Bundle;
        }
        _ => panic!("expected the new-session wizard"),
    }
    let answer = crate::test_support::availability_answer(&dashboard, &["docker"], &[]);
    dashboard.apply_availability_check(generation, Ok(answer));
    let wizard = new_wizard(&dashboard);
    assert_eq!(wizard.step, WizardStep::Target);
    assert_ne!(nth_key(&dashboard.config.targets, wizard.target), "docker");
    assert!(
        dashboard
            .notice()
            .is_some_and(|notice| notice.contains("docker is no longer available")),
        "{:?}",
        dashboard.notice()
    );
}

/// The fixture's targets plus the other standard local ones that
/// `Config::with_local_targets` supplies on Linux: `localhost`, `podman` and
/// `docker`.
fn standard_local_targets_config() -> mj_core::config::Config {
    let mut config = config();
    let TargetTemplate::LocalPodman { container } = config.targets["podman"].clone() else {
        panic!("the fixture's podman target is a Podman container");
    };
    config
        .targets
        .insert("localhost".into(), TargetTemplate::LocalBare);
    config
        .targets
        .insert("docker".into(), TargetTemplate::LocalDocker { container });
    // As `Config::with_local_targets` supplies them, not as the user wrote.
    config.default_targets = ["localhost", "podman", "docker"].map(str::to_owned).into();
    config
}

/// A bare SSH target, which has no size to set.
fn bare_ssh_target() -> TargetTemplate {
    TargetTemplate::SshBare {
        ssh: SshConnection {
            host: "builder.example.com".into(),
            user: None,
            identity_file: None,
            extra_args: Vec::new(),
        },
        permissions: mj_core::config::PermissionMode::Guardian,
        workspace_prefix: ".local/share/hel/workspaces".into(),
    }
}

/// Answers the availability check a wizard asks for when it opens: the
/// engines of the targets in `uninstalled` were never installed here, and
/// everything else is there.
fn answer_target_checks(dashboard: &mut DashboardState, uninstalled: &[&str]) {
    crate::test_support::answer_availability(dashboard, |dashboard| {
        crate::test_support::availability_answer(dashboard, uninstalled, &[])
    });
}

/// Activates the open wizard's Back button from the keyboard: Tab from
/// Cancel, which comes just before it, then Enter.
fn press_back(dashboard: &mut DashboardState) -> DashboardAction {
    match &mut dashboard.mode {
        Mode::New(wizard) => wizard.form.get_mut().focus(WizardControl::Cancel),
        Mode::Resume(wizard) => wizard.form.get_mut().focus(WizardControl::Cancel),
        _ => panic!("expected a wizard"),
    }
    dashboard.handle_key(key(KeyCode::Tab));
    let focused = match &dashboard.mode {
        Mode::New(wizard) => wizard.form.borrow().focused(),
        Mode::Resume(wizard) => wizard.form.borrow().focused(),
        _ => None,
    };
    assert_eq!(focused, Some(WizardControl::Back));
    dashboard.handle_key(key(KeyCode::Enter))
}

fn new_wizard(dashboard: &DashboardState) -> &NewWizard {
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected the new-session wizard");
    };
    wizard
}

fn new_session_prefers_interactive_choices_over_newer_unattended_sessions() {
    let mut config = config();
    config
        .targets
        .insert("docker".into(), config.targets["podman"].clone());
    config
        .bundles
        .insert("interactive-project".into(), config.bundles["hel"].clone());
    let latest = stopped_session();
    let state = State {
        sessions: [(latest.id.clone(), latest)].into_iter().collect(),
        ..State::default()
    };
    let mut dashboard = DashboardState::new(config, state, BTreeMap::new());
    dashboard.set_recent_launch(Some(mj_core::go::RecentLaunch {
        profile_id: "claude-1".into(),
        target_id: "docker".into(),
        bundle_id: "interactive-project".into(),
        project_directory: None,
    }));
    // Project ordering still follows session history, independently of selection.
    assert_eq!(
        bundle_ids_by_recent_creation(&dashboard.config, &dashboard.launch_recency)[0],
        "hel"
    );
    open_new_session_wizard(&mut dashboard);
    let wizard = new_wizard(&dashboard);
    assert_eq!(
        dashboard
            .config
            .enabled_profiles()
            .nth(wizard.profile)
            .unwrap()
            .0,
        "claude-1"
    );
    assert_eq!(nth_key(&dashboard.config.targets, wizard.target), "docker");
    assert_eq!(
        nth_bundle_key(&dashboard.config, &dashboard.launch_recency, wizard.bundle),
        "interactive-project"
    );
    captured_drawn(&mut dashboard, 140, 40);
    dashboard.handle_key(key(KeyCode::Enter));
    captured_drawn(&mut dashboard, 140, 40);
}

/// A machine no runtime runs on has no row, so the target step is kept to
/// name it and say why, even when one target is all it offers.
fn a_machine_without_a_runtime_keeps_the_target_step_and_says_why() {
    let mut config = standard_local_targets_config();
    let nas = mj_core::config::Machine::Ssh {
        ssh: SshConnection {
            host: "nas.example.test".into(),
            user: Some("dev".into()),
            identity_file: None,
            extra_args: vec![],
        },
        workspace_prefix: mj_core::config::DEFAULT_WORKSPACE_PREFIX.into(),
        build_cache: None,
    };
    config.machines.insert("nas".into(), nas);
    let mut dashboard = DashboardState::new(config, State::default(), BTreeMap::new());
    open_new_session_wizard(&mut dashboard);
    crate::test_support::answer_availability(&mut dashboard, |dashboard| {
        crate::test_support::availability_answer(dashboard, &["podman", "docker"], &["nas"])
    });
    dashboard.handle_key(key(KeyCode::Enter));
    let wizard = new_wizard(&dashboard);
    assert_eq!(wizard.step, WizardStep::Target);
    assert!(!wizard.target_step_skipped);
    let target = captured_drawn(&mut dashboard, 140, 40).join("\n");
    assert!(
        target.contains("Machine nas has no runtime: it did not answer."),
        "{target}"
    );
}

/// On a host without Podman or Docker, localhost is the only target the
/// target step offers, so the wizard chooses it and goes from the profile
/// step to the project step. The titles count three steps, Back skips the
/// hidden step, and the review names the target.
fn new_session_skips_the_target_step_when_only_one_target_is_offered() {
    // The last session ran on podman, so the draft starts there.
    let session = stopped_session();
    let state = State {
        sessions: [(session.id.clone(), session)].into_iter().collect(),
        ..State::default()
    };
    let mut dashboard =
        DashboardState::new(standard_local_targets_config(), state, BTreeMap::new());
    open_new_session_wizard(&mut dashboard);
    assert_eq!(
        nth_key(&dashboard.config.targets, new_wizard(&dashboard).target),
        "podman"
    );
    answer_target_checks(&mut dashboard, &["podman", "docker"]);
    let profile = captured_drawn(&mut dashboard, 140, 40).join("\n");
    assert!(profile.contains("New session · 1/3 profile"), "{profile}");

    dashboard.handle_key(key(KeyCode::Enter));
    let wizard = new_wizard(&dashboard);
    assert_eq!(wizard.step, WizardStep::ProjectDirectory);
    assert!(wizard.target_step_skipped);
    assert_eq!(
        nth_key(&dashboard.config.targets, wizard.target),
        "localhost"
    );
    let project = captured_drawn(&mut dashboard, 140, 40).join("\n");
    assert!(
        project.contains("New session · 2/3 local project"),
        "{project}"
    );

    press_back(&mut dashboard);
    assert_eq!(new_wizard(&dashboard).step, WizardStep::Profile);

    dashboard.handle_key(key(KeyCode::Enter));
    assert_eq!(new_wizard(&dashboard).step, WizardStep::ProjectDirectory);
    dashboard.handle_paste("/work/project");
    let action = dashboard.handle_key(key(KeyCode::Enter));
    capture_action(&action);
    assert_eq!(
        action,
        DashboardAction::ValidateProjectDirectory {
            target_template_id: "localhost".into(),
            directory: "/work/project".into(),
        }
    );
    // A folder without Git commits while review is on: the review page says
    // the session will be created with review off.
    dashboard.apply_resolved_project_directory(
        &dashboard.path_input_context(),
        "/work/project",
        Ok((
            PathBuf::from("/work/project"),
            mj_core::state::ManagedWorktreeOptions::default(),
            true,
        )),
    );
    assert_eq!(new_wizard(&dashboard).step, WizardStep::Review);
    let review = captured_drawn(&mut dashboard, 140, 40).join("\n");
    assert!(review.contains("New session · 3/3 review"), "{review}");
    assert!(review.contains("Target: localhost"), "{review}");

    press_back(&mut dashboard);
    assert_eq!(new_wizard(&dashboard).step, WizardStep::ProjectDirectory);
    press_back(&mut dashboard);
    assert_eq!(new_wizard(&dashboard).step, WizardStep::Profile);
}

/// A second target the step could offer keeps the step: one that is there,
/// and one no availability answer has reached yet.
fn new_session_shows_the_target_step_when_a_second_target_may_be_chosen() {
    for podman_ready in [true, false] {
        let mut dashboard = DashboardState::new(
            standard_local_targets_config(),
            State::default(),
            BTreeMap::new(),
        );
        open_new_session_wizard(&mut dashboard);
        // A check that has not answered leaves every target offered.
        if podman_ready {
            answer_target_checks(&mut dashboard, &["docker"]);
        }
        let profile = captured_drawn(&mut dashboard, 140, 40).join("\n");
        assert!(profile.contains("New session · 1/4 profile"), "{profile}");

        dashboard.handle_key(key(KeyCode::Enter));
        let wizard = new_wizard(&dashboard);
        assert_eq!(
            wizard.step,
            WizardStep::Target,
            "podman ready: {podman_ready}"
        );
        assert!(!wizard.target_step_skipped);
        let target = captured_drawn(&mut dashboard, 140, 40).join("\n");
        assert!(target.contains("New session · 2/4 target"), "{target}");
    }
}

/// A container target is sized on the target step, so even as the only
/// target its step is shown.
fn a_lone_container_target_keeps_its_step_for_sizing() {
    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    open_new_session_wizard(&mut dashboard);
    answer_target_checks(&mut dashboard, &[]);

    dashboard.handle_key(key(KeyCode::Enter));

    assert_eq!(new_wizard(&dashboard).step, WizardStep::Target);
    let target = captured_drawn(&mut dashboard, 140, 40).join("\n");
    assert!(target.contains("New session · 2/4 target"), "{target}");
}

/// A stopped session that opens a directory on an SSH host, with that host's
/// bare target added to the standard local ones.
fn dashboard_with_raw_ssh_session(state: SessionState) -> DashboardState {
    let mut session = stopped_session();
    session.state = state;
    session.target_template_id = "machine".into();
    session.project_directory = Some("/srv/project".into());
    let mut dashboard = dashboard_with_session(session);
    *dashboard.config = standard_local_targets_config();
    dashboard
        .config
        .targets
        .insert("machine".into(), bare_ssh_target());
    dashboard
}

/// A session that opens a directory on an SSH host can only resume on a bare
/// target there. Every other target is ready, but it cannot use them, so the
/// wizard chooses the SSH target without showing the step.
fn resume_skips_the_target_step_when_only_one_target_suits_the_session() {
    let mut dashboard = dashboard_with_raw_ssh_session(SessionState::Stopped);
    let action = dashboard.begin_resume_for("session-1");
    capture_action(&action);
    assert_eq!(action, DashboardAction::None);
    answer_target_checks(&mut dashboard, &[]);
    let profile = captured_drawn(&mut dashboard, 140, 40).join("\n");
    assert!(profile.contains("Resume · 1/2 profile"), "{profile}");

    dashboard.handle_key(key(KeyCode::Enter));
    let wizard = resume_wizard(&dashboard);
    assert_eq!(wizard.step, WizardStep::Review);
    assert!(wizard.target_step_skipped);
    assert_eq!(nth_key(&dashboard.config.targets, wizard.target), "machine");
    let review = captured_drawn(&mut dashboard, 140, 40).join("\n");
    assert!(review.contains("Resume · 2/2 review"), "{review}");
    assert!(review.contains("Target: machine"), "{review}");

    press_back(&mut dashboard);
    assert_eq!(resume_wizard(&dashboard).step, WizardStep::Profile);

    dashboard.handle_key(key(KeyCode::Enter));
    assert_eq!(resume_wizard(&dashboard).step, WizardStep::Review);
    let action = dashboard.handle_key(key(KeyCode::Enter));
    capture_action(&action);
    let DashboardAction::PreflightResumeRepositories { launch } = action else {
        panic!("Resume on the review starts the resume");
    };
    assert!(matches!(
        launch.as_ref(),
        DashboardAction::ResumeSession { target_template_id, .. } if target_template_id == "machine"
    ));
}

/// A checkout on this machine can resume in place or move into a container,
/// so a ready podman is a second choice and the step is shown.
fn resume_shows_the_target_step_when_two_targets_suit_the_session() {
    let mut session = stopped_session();
    session.target_template_id = "localhost".into();
    session.project_directory = Some("/work/project".into());
    let mut dashboard = dashboard_with_session(session);
    *dashboard.config = standard_local_targets_config();
    let action = dashboard.begin_resume_for("session-1");
    capture_action(&action);
    assert_eq!(action, DashboardAction::None);
    answer_target_checks(&mut dashboard, &["docker"]);
    let profile = captured_drawn(&mut dashboard, 140, 40).join("\n");
    assert!(profile.contains("Resume · 1/3 profile"), "{profile}");

    dashboard.handle_key(key(KeyCode::Enter));

    let wizard = resume_wizard(&dashboard);
    assert_eq!(wizard.step, WizardStep::Target);
    assert!(!wizard.target_step_skipped);
    let target = captured_drawn(&mut dashboard, 140, 40).join("\n");
    assert!(target.contains("Resume · 2/3 new target"), "{target}");
}

/// Move uses the resume wizard's rule: a live session on an SSH host can only
/// move to a bare target there, so the step is skipped and the move is
/// prepared for that target at once.
fn move_skips_the_target_step_when_only_one_target_suits_the_session() {
    let mut dashboard = dashboard_with_raw_ssh_session(SessionState::Running);
    dashboard.focus_sessions();
    let action = dashboard.begin_move();
    capture_action(&action);
    assert_eq!(action, DashboardAction::None);
    answer_target_checks(&mut dashboard, &[]);
    let profile = captured_drawn(&mut dashboard, 140, 40).join("\n");
    assert!(profile.contains("Move · 1/2 profile"), "{profile}");

    let preparation = dashboard.handle_key(key(KeyCode::Enter));
    capture_action(&preparation);
    assert!(
        matches!(
            &preparation,
            DashboardAction::MoveSession {
                target_template_id,
                preparation_request_id: Some(_),
                ..
            } if target_template_id == "machine"
        ),
        "{preparation:?}"
    );
    let wizard = resume_wizard(&dashboard);
    assert_eq!(wizard.step, WizardStep::Review);
    assert!(wizard.target_step_skipped);
    let review = captured_drawn(&mut dashboard, 140, 40).join("\n");
    assert!(review.contains("Move · 2/2 confirm"), "{review}");
    assert!(review.contains("Target: machine"), "{review}");

    press_back(&mut dashboard);
    let wizard = resume_wizard(&dashboard);
    assert_eq!(wizard.step, WizardStep::Profile);
    assert!(
        !wizard.preparing,
        "leaving the review drops its preparation"
    );
}

/// A live checkout on this machine can also move into a ready podman, so the
/// Move wizard shows its target step.
fn move_shows_the_target_step_when_two_targets_suit_the_session() {
    let mut session = running_session();
    session.target_template_id = "localhost".into();
    session.project_directory = Some("/work/project".into());
    let mut dashboard = dashboard_with_session(session);
    *dashboard.config = standard_local_targets_config();
    dashboard.focus_sessions();
    let action = dashboard.begin_move();
    capture_action(&action);
    assert_eq!(action, DashboardAction::None);
    answer_target_checks(&mut dashboard, &["docker"]);
    let profile = captured_drawn(&mut dashboard, 140, 40).join("\n");
    assert!(profile.contains("Move · 1/3 profile"), "{profile}");

    assert_eq!(
        dashboard.handle_key(key(KeyCode::Enter)),
        DashboardAction::None
    );

    let wizard = resume_wizard(&dashboard);
    assert_eq!(wizard.step, WizardStep::Target);
    assert!(!wizard.target_step_skipped);
    let target = captured_drawn(&mut dashboard, 140, 40).join("\n");
    assert!(target.contains("Move · 2/3 new target"), "{target}");
}

/// Answers the availability check the way the dashboard loop does on a host
/// with no Docker: `docker` was never installed, `machine` did not answer,
/// and everything else is there.
fn answer_checks_without_docker(dashboard: &mut DashboardState) {
    crate::test_support::answer_availability(dashboard, |dashboard| {
        crate::test_support::availability_answer(dashboard, &["docker"], &["machine"])
    });
}

/// The machine `bare_ssh_target` runs on.
fn bare_ssh_machine() -> mj_core::config::Machine {
    let TargetTemplate::SshBare {
        ssh,
        workspace_prefix,
        ..
    } = bare_ssh_target()
    else {
        unreachable!("a bare SSH target");
    };
    mj_core::config::Machine::Ssh {
        ssh,
        workspace_prefix,
        build_cache: None,
    }
}

/// The user's rule: the pickers list only what is there. A standard runtime
/// whose engine was never installed is left out without a word; a runtime
/// whose host did not answer is left out and named under the list with the
/// reason. The step shows the same rows for New, Resume and Move.
#[test]
fn target_steps_list_only_what_is_there_and_name_an_unresponsive_host() {
    for flow in ["New session", "Resume", "Move"] {
        let mut session = if flow == "Move" {
            running_session()
        } else {
            stopped_session()
        };
        session.target_template_id = "localhost".into();
        session.project_directory = Some("/work/project".into());
        let mut dashboard = dashboard_with_session(session);
        *dashboard.config = standard_local_targets_config();
        dashboard
            .config
            .targets
            .insert("machine".into(), bare_ssh_target());
        dashboard
            .config
            .machines
            .insert("machine".into(), bare_ssh_machine());
        match flow {
            "New session" => {
                open_new_session_wizard(&mut dashboard);
            }
            "Resume" => {
                assert_eq!(
                    dashboard.begin_resume_for("session-1"),
                    DashboardAction::None
                );
            }
            _ => {
                dashboard.focus_sessions();
                assert_eq!(dashboard.begin_move(), DashboardAction::None);
            }
        }
        answer_checks_without_docker(&mut dashboard);
        dashboard.handle_key(key(KeyCode::Enter));
        let text = captured_drawn(&mut dashboard, 160, 40).join("\n");
        assert!(text.contains("target"), "{flow}: {text}");
        assert!(text.contains("podman"), "{flow}: {text}");
        assert!(
            text.contains("machine: machine did not answer.") && !text.contains("unavailable"),
            "{flow}: an unresponsive host is named, not listed: {text}"
        );
        assert!(
            !text.contains("docker"),
            "{flow}: a missing runtime is not listed: {text}"
        );
        assert!(
            !text.contains("Docker is not installed"),
            "{flow}: nor is its reason: {text}"
        );
    }
}

/// Moving the selection lands on the next listed target, not the next config
/// entry, because the hidden target shifts the rows.
fn selecting_a_row_skips_the_hidden_target() {
    let mut dashboard = DashboardState::new(
        standard_local_targets_config(),
        State::default(),
        BTreeMap::new(),
    );
    open_new_session_wizard(&mut dashboard);
    answer_checks_without_docker(&mut dashboard);
    dashboard.handle_key(key(KeyCode::Enter));
    // Config order is docker, localhost, podman; the rows are the last two.
    let position = |dashboard: &DashboardState, id: &str| {
        dashboard
            .config
            .targets
            .keys()
            .position(|key| key == id)
            .unwrap()
    };
    let localhost = position(&dashboard, "localhost");
    let podman = position(&dashboard, "podman");
    let Mode::New(wizard) = &mut dashboard.mode else {
        panic!("expected the new-session wizard");
    };
    wizard.target = localhost;
    captured_drawn(&mut dashboard, 160, 40);
    dashboard.handle_key(key(KeyCode::Down));
    assert_eq!(new_wizard(&dashboard).target, podman);
    capture_detail(
        "selection",
        "podman (the next visible row; docker is hidden)",
    );
    captured_drawn(&mut dashboard, 160, 40);
}

fn ctrl_space() -> KeyEvent {
    KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL)
}

/// A bare SSH target owns the project directory, so its completions come from
/// the target's own machine rather than the controller.
#[test]
fn project_directory_completes_on_the_target_host() {
    let mut config = config();
    config.targets = BTreeMap::from([(
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
    )]);
    let mut dashboard = DashboardState::new(config, State::default(), BTreeMap::new());
    ready_open_new_wizard(&mut dashboard);
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    assert!(matches!(
        &dashboard.mode,
        Mode::New(wizard) if wizard.step == WizardStep::ProjectDirectory
    ));
    for character in "/srv/p".chars() {
        ready_key(&mut dashboard, key(KeyCode::Char(character)));
    }

    assert_eq!(
        ready_key(&mut dashboard, ctrl_space()),
        DashboardAction::CompletePath {
            host: CompletionHost::Target("remote".into()),
            kind: CompletionKind::Directories,
            prefix: "/srv/p".into(),
        }
    );
    dashboard.apply_path_completions(
        &dashboard.path_input_context(),
        "/srv/p",
        PathCompletion {
            candidates: vec!["/srv/project/".into(), "/srv/proxy/".into()],
            insert: Some("/srv/pro".into()),
            truncated: false,
        },
    );
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected the project directory step");
    };
    assert_eq!(wizard.project_directory, "/srv/pro");
    assert!(wizard.project_directory.is_completing());

    ready_key(&mut dashboard, key(KeyCode::Down));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected the project directory step");
    };
    assert_eq!(wizard.project_directory, "/srv/proxy/");
}

/// A reply that arrives after another keystroke belongs to text the field no
/// longer holds, so it is dropped without touching what was typed.
#[test]
fn stale_completion_reply_is_dropped() {
    let mut dashboard = dashboard_at_mount_editor("/opt");
    let Mode::New(wizard) = &mut dashboard.mode else {
        panic!("expected directory editor");
    };
    wizard.form.get_mut().focus(WizardControl::MountSource);
    assert!(matches!(
        ready_key(&mut dashboard, ctrl_space()),
        DashboardAction::CompletePath { .. }
    ));
    let context = dashboard.path_input_context();
    ready_key(&mut dashboard, key(KeyCode::Char('e')));

    dashboard.apply_path_completions(
        &context,
        "/opt",
        PathCompletion {
            candidates: vec!["/opt/one/".into(), "/opt/two/".into()],
            insert: Some("/opt/".into()),
            truncated: false,
        },
    );
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected directory editor");
    };
    assert_eq!(wizard.mounts.source, "/opte");
    assert!(!wizard.mounts.source.is_completing());
}

/// A dashboard on the local project step of the New wizard, with `history`
/// already remembered for this host.
fn dashboard_at_local_project_step(history: &[&str]) -> DashboardState {
    let mut config = config();
    config.targets = BTreeMap::from([("localhost".into(), TargetTemplate::LocalBare)]);
    let mut state = State::default();
    for directory in history {
        state.remember_project_directory("local", std::path::Path::new(directory));
    }
    let mut dashboard = DashboardState::new(config, state, BTreeMap::new());
    ready_open_new_wizard(&mut dashboard);
    // localhost is the only target, so its step is passed.
    ready_key(&mut dashboard, key(KeyCode::Enter));
    assert!(
        matches!(&dashboard.mode, Mode::New(wizard) if wizard.step == WizardStep::ProjectDirectory),
        "expected the local project step"
    );
    dashboard
}

/// The row of `lines` holding `label`, panicking with the screen when it is
/// nowhere on it.
fn row_of(lines: &[String], label: &str) -> usize {
    lines
        .iter()
        .position(|line| line.contains(label))
        .unwrap_or_else(|| panic!("missing {label:?}: {lines:#?}"))
}

/// `cd ~/demo && mj`: the local project step starts with the repository `mj`
/// was started in, ahead of any remembered directory (J-10).
// Hard-won: a16ff9c6: the prefilled repository path was hidden, so Enter launched there without notice
#[test]
fn the_local_project_starts_as_the_repository_mj_was_started_in() {
    let mut config = config();
    config.targets = BTreeMap::from([("localhost".into(), TargetTemplate::LocalBare)]);
    let mut state = State::default();
    state.remember_project_directory("local", std::path::Path::new("/work/remembered"));
    let mut dashboard = DashboardState::new(config, state, BTreeMap::new());
    dashboard.set_launch_project_directory(Some("/home/me/demo".into()));
    ready_open_new_wizard(&mut dashboard);
    // localhost is the only target, so its step is passed.
    ready_key(&mut dashboard, key(KeyCode::Enter));
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected the local project step")
    };
    assert_eq!(wizard.step, WizardStep::ProjectDirectory);
    assert_eq!(wizard.project_directory, "/home/me/demo");
    // Launch finding R1-1: the step says where the path came from, so a
    // plain Enter is a visible choice of that repository.
    let draw = |dashboard: &mut DashboardState| {
        let mut terminal = Terminal::new(TestBackend::new(140, 36)).unwrap();
        terminal.draw(|frame| render(frame, dashboard)).unwrap();
        buffer_lines(terminal.backend().buffer()).join("\n")
    };
    let step = draw(&mut dashboard);
    assert!(
        step.contains("Filled in with the repository mj was started in"),
        "{step}"
    );
    ready_key(&mut dashboard, key(KeyCode::Enter));
    dashboard.apply_project_directory_validation("/home/me/demo", Ok(()));
    assert!(matches!(&dashboard.mode, Mode::New(wizard) if wizard.step == WizardStep::Review));
    let review = draw(&mut dashboard);
    assert!(
        review.contains("/home/me/demo  (the repository mj was started in)"),
        "{review}"
    );
}

/// The step pre-fills a remembered directory, so it has to draw it with the
/// caret at its end. A value the field holds but does not show takes every
/// character typed after it and turns the path into two paths.
#[test]
fn a_remembered_project_directory_is_drawn_with_the_caret_at_its_end() {
    let mut dashboard = dashboard_at_local_project_step(&["/work/remembered"]);

    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected the local project step")
    };
    assert_eq!(wizard.project_directory, "/work/remembered");
    assert_eq!(
        wizard.project_directory.cursor(),
        "/work/remembered".len(),
        "typing must continue at the end of what is drawn"
    );

    let lines = captured_drawn(&mut dashboard, 140, 40);
    let field = row_of(&lines, "New session · 2/3 local project") + 4;
    assert!(
        lines[field].contains("/work/remembered"),
        "the field draws its value: {:?}",
        lines[field]
    );
}

/// Everything the step composes has to be on the screen. The remembered
/// directories and the key hints are the last rows it writes, so a dialog that
/// is not sized for its own field and buttons loses exactly those.
// Hard-won: ee7ee201: project details, recents, or errors were hidden under wizard controls
#[test]
fn the_project_step_draws_its_recent_list_and_error_above_the_buttons() {
    let mut dashboard = dashboard_at_local_project_step(&["/work/newer", "/work/older"]);
    for character in "/work/newer".chars() {
        ready_key(&mut dashboard, key(KeyCode::Char(character)));
    }
    ready_key(&mut dashboard, key(KeyCode::Enter));
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected the local project step")
    };
    let typed = wizard.project_directory.to_string();
    dashboard.apply_project_directory_validation(
        &typed,
        Err("project directory does not exist or is not a directory".into()),
    );

    let lines = captured_drawn(&mut dashboard, 140, 40);
    assert_dialog_spacing(&lines, "New session", "Cancel");
    let buttons = row_of(&lines, "Cancel");
    for label in [
        "Error: project directory does not exist",
        "Recent on this host",
        "/work/older",
    ] {
        assert!(
            row_of(&lines, label) < buttons,
            "{label:?} is drawn above the button row: {lines:#?}"
        );
    }
}

fn clicking_a_recent_project_fills_the_field_and_enter_validates_it() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let mut dashboard = dashboard_at_local_project_step(&["/work/older", "/work/newer"]);
    if let Mode::New(wizard) = &mut dashboard.mode {
        wizard.project_directory_error = Some("previous path was invalid".into());
    }
    let lines = captured_drawn(&mut dashboard, 140, 40);
    let row = row_of(&lines, "/work/older");
    let column = lines[row].find("/work/older").unwrap();
    let column = lines[row][..column].chars().count() as u16;
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        assert_eq!(
            dashboard.handle_mouse(MouseEvent {
                kind,
                column,
                row: row as u16,
                modifiers: KeyModifiers::NONE,
            }),
            DashboardAction::None
        );
        captured_drawn(&mut dashboard, 140, 40);
    }
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("clicking a recent directory stays on the project step")
    };
    assert_eq!(wizard.project_directory, "/work/older");
    assert_eq!(wizard.project_directory.cursor(), "/work/older".len());
    assert_eq!(wizard.project_history_index, 1);
    assert!(wizard.project_directory_error.is_none());
    assert_eq!(
        wizard.form.borrow().focused(),
        Some(WizardControl::ProjectDirectory)
    );
    assert!(matches!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::ValidateProjectDirectory { .. }
    ));
}

/// The completion popup belongs to the dialog that owns the field: it may cover
/// the rows under the field, but the button row and the dialog's frame stay
/// readable, and no candidate is drawn on the dashboard behind the modal.
// Hard-won: 792e7ede: completion suggestions covered Cancel, Back, and Next
#[test]
fn the_completion_popup_keeps_off_the_project_steps_button_row() {
    let mut dashboard = dashboard_at_local_project_step(&[]);
    for character in "/srv/pro".chars() {
        ready_key(&mut dashboard, key(KeyCode::Char(character)));
    }
    assert_eq!(
        ready_key(&mut dashboard, ctrl_space()),
        DashboardAction::CompletePath {
            host: CompletionHost::Target("localhost".into()),
            kind: CompletionKind::Directories,
            prefix: "/srv/pro".into(),
        }
    );
    dashboard.apply_path_completions(
        &dashboard.path_input_context(),
        "/srv/pro",
        PathCompletion {
            candidates: vec!["/srv/profile/".into(), "/srv/project/".into()],
            insert: None,
            truncated: false,
        },
    );

    let lines = captured_drawn(&mut dashboard, 140, 40);
    let field = row_of(&lines, "New session · 2/3 local project") + 4;
    let buttons = row_of(&lines, "Cancel");
    assert!(
        lines[field].contains("/srv/pro"),
        "the popup leaves the field readable: {:?}",
        lines[field]
    );
    for candidate in ["/srv/profile/", "/srv/project/"] {
        let row = row_of(&lines, candidate);
        assert!(
            row > field && row < buttons,
            "{candidate} is drawn between the field and the buttons: {lines:#?}"
        );
        assert_eq!(
            lines.iter().filter(|line| line.contains(candidate)).count(),
            1,
            "{candidate} is drawn once: {lines:#?}"
        );
    }
    assert!(
        lines[buttons].contains("Back") && lines[buttons].contains("Next"),
        "the button row is whole: {:?}",
        lines[buttons]
    );
    let title = &lines[row_of(&lines, "New session · 2/3 local project")];
    let modal_left = title.chars().position(|c| c == '╭').unwrap();
    let modal_width = title
        .chars()
        .skip(modal_left + 1)
        .position(|c| c == '╮')
        .unwrap();
    assert!(
        !lines[buttons]
            .chars()
            .skip(modal_left + 1)
            .take(modal_width)
            .any(|c| c == '\u{2500}'),
        "no popup border is drawn through the buttons: {:?}",
        lines[buttons]
    );
}

/// The review's Compute row states the allocation itself; the " · " that
/// joins it to a target name in the target list does not belong there.
/// Launch campaign finding C-7.
// Hard-won: b87a404a: review rendered an empty compute value with a stray separator
#[test]
fn the_review_compute_row_does_not_start_with_a_separator() {
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
    let _ = dashboard.take_prerequisite_check();
    let context = dashboard.path_input_context();
    dashboard.apply_resolved_project_directory(
        &context,
        "/work/main",
        Ok((
            PathBuf::from("/work/main"),
            mj_core::state::ManagedWorktreeOptions {
                available: false,
                default_create: false,
            },
            false,
        )),
    );

    let lines = captured_drawn(&mut dashboard, 120, 40);
    let compute = &lines[row_of(&lines, "Compute:")];
    assert!(
        compute.contains("Compute: fixed/default resources"),
        "{compute:?}"
    );

    let Mode::New(wizard) = &mut dashboard.mode else {
        panic!("new wizard")
    };
    wizard.resource_allocation = Some(SessionResourceAllocation::Container {
        cpus: 2,
        memory_bytes: 4 * 1024 * 1024 * 1024,
    });
    let lines = captured_drawn(&mut dashboard, 120, 40);
    let compute = &lines[row_of(&lines, "Compute:")];
    assert!(compute.contains("Compute: 2 CPU / "), "{compute:?}");
}

/// Another surface or this run's own sessions can remember a project directory
/// after the dashboard loaded its state. Opening the New wizard asks for the
/// stored history again, and the project step offers what comes back.
/// Launch campaign finding C-1.
// Hard-won: 5a8c036f: a project created in one run was missing from the next picker until restart
#[test]
fn opening_the_new_wizard_refreshes_recent_projects() {
    let mut config = config();
    config.targets = BTreeMap::from([("localhost".into(), TargetTemplate::LocalBare)]);
    let mut dashboard = DashboardState::new(config, State::default(), BTreeMap::new());
    ready_open_new_wizard(&mut dashboard);
    assert_eq!(
        dashboard.take_mount_history_refresh(),
        Some(DashboardAction::LoadMountHistory)
    );
    assert_eq!(
        dashboard.take_mount_history_refresh(),
        None,
        "one open asks once"
    );
    let mut stored = State::default();
    stored.remember_project_directory("local", std::path::Path::new("/work/used-this-run"));
    dashboard.apply_mount_history(stored.mount_history);

    // localhost is the only target, so its step is passed.
    ready_key(&mut dashboard, key(KeyCode::Enter));
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("expected the New wizard")
    };
    assert_eq!(wizard.step, WizardStep::ProjectDirectory);
    assert_eq!(
        wizard.project_history,
        vec![PathBuf::from("/work/used-this-run")]
    );
}

#[test]
fn recent_projects_update_an_open_picker_and_failed_discovery_has_an_explicit_retry() {
    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    ready_open_new_wizard(&mut dashboard);
    let Mode::New(mut wizard) = std::mem::replace(&mut dashboard.mode, Mode::Dashboard) else {
        panic!("new wizard");
    };
    wizard.open_projects(&dashboard);
    dashboard.mode = Mode::New(wizard);
    assert_eq!(
        dashboard.take_project_discovery(),
        Some(DashboardAction::LoadMountHistory)
    );
    let mut history = State::default();
    history.remember_project_directory("local", std::path::Path::new("/work/discovered"));
    dashboard.apply_mount_history(history.mount_history);
    dashboard.apply_project_catalog_status(
        mj_core::project_catalog::ProjectCatalogStatus::Failed {
            errors: vec![
                format!("missing native project: {}", "stale checkout; ".repeat(100)),
                "fatal: not a git repository".into(),
            ],
        },
    );
    let Mode::New(wizard) = &dashboard.mode else {
        panic!("new wizard")
    };
    assert!(
        wizard
            .project_picker
            .entries
            .iter()
            .any(|entry| entry.source == "/work/discovered")
    );
    assert_eq!(
        wizard.project_picker.error.as_deref(),
        Some("Project discovery reported 2 errors. See daemon logs for details.")
    );
    let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .unwrap();
    let rendered = buffer_lines(terminal.backend().buffer()).join("\n");
    assert!(
        rendered.contains("Project discovery reported 2 errors."),
        "{rendered}"
    );
    assert!(!rendered.contains("stale checkout"), "{rendered}");
    assert!(!rendered.contains("fatal:"), "{rendered}");
    activate_project_control(&mut dashboard, WizardControl::ProjectRetry);
    assert_eq!(
        dashboard.take_project_discovery(),
        Some(DashboardAction::RefreshProjects { retry: true })
    );
    assert_eq!(dashboard.take_project_discovery(), None);
}

/// Tab moves keyboard focus onto the recent directories, so each one has to
/// look different while it holds focus, including the one that is already the
/// selected directory. Launch campaign finding C-2.
// Hard-won: 8e3e20d4: focused and unfocused recent rows rendered identically
#[test]
fn a_focused_recent_project_is_drawn_differently_from_its_unfocused_self() {
    fn style_of(dashboard: &mut DashboardState, label: &str) -> ratatui::style::Style {
        let mut terminal = Terminal::new(TestBackend::new(140, 40)).expect("terminal");
        terminal
            .draw(|frame| render(frame, dashboard))
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let (column, row) = point(&buffer_lines(&buffer), label);
        buffer[(column, row)].style()
    }
    let labels = ["/work/newer", "/work/older"];
    let mut dashboard = dashboard_at_local_project_step(&labels);
    let unfocused = labels.map(|label| style_of(&mut dashboard, label));

    let mut checked = 0;
    for _ in 0..6 {
        ready_key(&mut dashboard, key(KeyCode::Tab));
        let Mode::New(wizard) = &dashboard.mode else {
            panic!("expected the local project step")
        };
        let Some(WizardControl::RecentProject(index)) = wizard.form.borrow().focused() else {
            continue;
        };
        let label = wizard.project_history[index].display().to_string();
        let position = labels.iter().position(|known| *known == label).unwrap();
        assert_ne!(
            style_of(&mut dashboard, &label),
            unfocused[position],
            "{label} looks the same with and without focus"
        );
        checked += 1;
    }
    assert!(checked >= 2, "Tab reaches both recent directories");
}

#[test]
fn large_move_opens_separate_file_page_with_directory_and_file_sizes() {
    use mj_core::move_workspace::*;
    let mut dashboard = dashboard_with_session(running_session());
    let request_id = open_move_review(&mut dashboard);
    let mut preparation = move_preparation();
    let mut assessment = WorkspaceAssessment::default();
    assessment.files = vec![
        WorkspaceFile {
            location: WorkspacePath {
                repository: "repo".into(),
                path: ".agents/qualification/java-b/staged/tool.bin".into(),
            },
            bytes: 1_000_000_000,
        },
        WorkspaceFile {
            location: WorkspacePath {
                repository: "repo".into(),
                path: ".agents/qualification/java-b/staged/readme.txt".into(),
            },
            bytes: 1234,
        },
    ];
    assessment.roots = file_tree(&assessment);
    assessment.initially_expanded = initial_expansion(&assessment.roots);
    preparation.workspace = Some(assessment);
    assert!(dashboard.apply_move_preparation(request_id, preparation));
    assert_eq!(resume_wizard(&dashboard).step, WizardStep::MoveFiles);
    let mut terminal = Terminal::new(TestBackend::new(160, 40)).unwrap();
    terminal
        .draw(|frame| render(frame, &mut dashboard))
        .unwrap();
    let rendered = buffer_lines(terminal.backend().buffer()).join(" ");
    assert_dialog_spacing(
        &buffer_lines(terminal.backend().buffer()),
        "Move · choose files",
        "Continue",
    );
    assert!(rendered.contains("Choose files to transfer"), "{rendered}");
    assert!(rendered.contains("1.00 GB"), "{rendered}");
    assert!(
        rendered.contains(".agents/qualification/java-b/staged"),
        "{rendered}"
    );
}

/// #1175: the target step is a table like the profile step, and the resize
/// keys are offered only for a target that is sized.
fn target_step_draws_a_table_and_offers_resize_keys_only_for_sized_targets() {
    let mut configuration = config();
    configuration
        .targets
        .insert("bare".into(), TargetTemplate::LocalBare);
    let index = |id: &str| {
        configuration
            .targets
            .keys()
            .position(|key| key == id)
            .unwrap()
    };
    let (bare, podman) = (index("bare"), index("podman"));
    let mut dashboard = DashboardState::new(configuration, State::default(), BTreeMap::new());
    dashboard.begin_new();
    let draw = |dashboard: &mut DashboardState, target: usize| {
        let Mode::New(wizard) = &mut dashboard.mode else {
            panic!("new session wizard")
        };
        wizard.step = WizardStep::Target;
        wizard.target = target;
        let mut terminal = Terminal::new(TestBackend::new(140, 32)).unwrap();
        terminal.draw(|frame| render(frame, dashboard)).unwrap();
        let lines = buffer_lines(terminal.backend().buffer());
        capture_render(140, 32, &lines);
        lines.join("\n")
    };

    let raw = draw(&mut dashboard, bare);
    assert!(raw.contains("TARGET") && raw.contains("STATUS"), "{raw}");
    assert!(!raw.contains("+ double"), "{raw}");
    assert!(!raw.contains("fixed/default resources"), "{raw}");

    let sized = draw(&mut dashboard, podman);
    assert!(!sized.contains("+ double"), "{sized}");
    assert!(
        sized.contains("CPU") && sized.contains("MEM (GiB)"),
        "{sized}"
    );
}

fn replace_resource_text(dashboard: &mut DashboardState, text: &str) {
    ready_key(dashboard, key(KeyCode::End));
    ready_key(
        dashboard,
        crossterm::event::KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
    );
    for character in text.chars() {
        ready_key(dashboard, key(KeyCode::Char(character)));
    }
}

fn container_fields_are_underlined_before_focus_and_clickable_on_small_terminals() {
    use crossterm::event::{MouseButton, MouseEventKind};
    for (width, height) in [(140, 40), (80, 24), (60, 20)] {
        let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
        ready_open_new_wizard(&mut dashboard);
        ready_key(&mut dashboard, key(KeyCode::Enter));
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| render(frame, &mut dashboard))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let lines = buffer_lines(buffer);
        capture_render(width, height, &lines);
        capture_detail(
            "cell style",
            "CPU and MEM (GiB) inputs are underlined before focus",
        );
        let y = row_of(&lines, "MEM (GiB)") as u16 + 1;
        let (cpu_x, _) = point(&lines, "CPU");
        let (memory_x, _) = point(&lines, "MEM (GiB)");
        for (x, expected) in [
            (cpu_x, WizardControl::ResourceCpu),
            (memory_x, WizardControl::ResourceMemory),
        ] {
            assert!(
                buffer[(x, y)].modifier.contains(Modifier::UNDERLINED),
                "{width}x{height}: {lines:#?}"
            );
            dashboard.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), (x, y)));
            dashboard.handle_mouse(mouse_at(MouseEventKind::Up(MouseButton::Left), (x, y)));
            assert_eq!(
                new_wizard(&dashboard).form.borrow().focused(),
                Some(expected)
            );
            assert_eq!(new_wizard(&dashboard).step, WizardStep::Target);
        }
        replace_resource_text(&mut dashboard, "12.5");
        captured_drawn(&mut dashboard, width, height);
        assert_eq!(
            new_wizard(&dashboard).resource_allocation,
            Some(SessionResourceAllocation::Container {
                cpus: 4,
                memory_bytes: (12 << 30) + (1 << 29)
            })
        );
        assert_dialog_spacing(&lines, "target", "Next");
    }
}

#[test]
fn ec2_dropdown_selects_instances_preserves_refresh_and_rejects_removed_selection() {
    let aws = TargetTemplate::AwsEc2 {
        aws_profile: None,
        region: "us-east-1".into(),
        launch_template: "hel".into(),
        launch_template_version: None,
        ssh_user: "ubuntu".into(),
        address_source: mj_core::config::AwsAddressSource::PublicIp,
        identity_file: None,
        ssh_args: Vec::new(),
    };
    let mut configuration = config();
    configuration.targets = BTreeMap::from([("aws".into(), aws)]);
    let mut dashboard = DashboardState::new(configuration, State::default(), BTreeMap::new());
    ready_open_new_wizard(&mut dashboard);
    ready_key(&mut dashboard, key(KeyCode::Enter));
    assert!(new_wizard(&dashboard).resource_allocation.is_none());
    let loading = captured_drawn(&mut dashboard, 80, 24).join("\n");
    assert!(loading.contains("Loading instance types"), "{loading}");
    let options = [8, 16]
        .into_iter()
        .map(|vcpus| SessionResourceAllocation::AwsEc2 {
            instance_type: format!("family.{vcpus}"),
            vcpus,
            memory_bytes: vcpus * (4 << 30),
        })
        .collect::<Vec<_>>();
    dashboard.apply_aws_resource_options("aws", Ok(options.clone()));
    ready_key(&mut dashboard, key(KeyCode::Tab));
    assert_eq!(
        new_wizard(&dashboard).form.borrow().focused(),
        Some(WizardControl::ResourceInstance)
    );
    ready_key(&mut dashboard, key(KeyCode::Enter));
    let popup = captured_drawn(&mut dashboard, 80, 24).join("\n");
    assert!(popup.contains("Instance types"), "{popup}");
    ready_key(&mut dashboard, key(KeyCode::Down));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    assert_eq!(
        new_wizard(&dashboard).resource_allocation,
        Some(options[1].clone())
    );
    assert!(
        !new_wizard(&dashboard)
            .resource_editor
            .instances
            .is_open(WizardControl::ResourceInstance)
    );
    dashboard.apply_aws_resource_options("aws", Ok(options.clone()));
    assert_eq!(
        new_wizard(&dashboard).resource_allocation,
        Some(options[1].clone())
    );
    dashboard.apply_aws_resource_options("aws", Ok(vec![options[0].clone()]));
    dashboard.apply_aws_resource_options("aws", Ok(vec![options[0].clone()]));
    assert!(new_wizard(&dashboard).resource_allocation.is_none());
    assert!(
        new_wizard(&dashboard)
            .sizing_error
            .as_ref()
            .unwrap()
            .contains("unavailable")
    );
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    assert_eq!(
        new_wizard(&dashboard).resource_allocation,
        Some(options[0].clone())
    );
}

#[test]
fn container_memory_errors_keep_the_draft_and_refuse_submission_above_host_limits() {
    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
    dashboard.apply_deployment_capacity(
        "local",
        Ok(Some(mj_core::targets::DeploymentCapacityUsage {
            cpu_percent: None,
            memory_used_bytes: 0,
            memory_total_bytes: 16 << 30,
            logical_cores: 8,
            disk_total_bytes: None,
            storage: Vec::new(),
        })),
        0,
    );
    set_local_container_capacity(
        &mut dashboard,
        HostContainerSize {
            cpus: 8,
            memory_bytes: 16 << 30,
        },
        HostContainerSize {
            cpus: 0,
            memory_bytes: 0,
        },
        HostContainerSize {
            cpus: 8,
            memory_bytes: (16 << 30) * 3 / 5,
        },
        true,
    );
    ready_open_new_wizard(&mut dashboard);
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Tab));
    replace_resource_text(&mut dashboard, "9");
    assert_eq!(new_wizard(&dashboard).resource_editor.cpu.value(), "9");
    assert!(
        new_wizard(&dashboard)
            .sizing_error
            .as_ref()
            .unwrap()
            .contains("CPU exceeds")
    );
    replace_resource_text(&mut dashboard, "4");
    ready_key(&mut dashboard, key(KeyCode::Tab));
    for text in ["", "0", "NaN", "-1", "3", "20", "18446744073709551615"] {
        replace_resource_text(&mut dashboard, text);
        assert_eq!(new_wizard(&dashboard).resource_editor.memory.value(), text);
        assert!(new_wizard(&dashboard).resource_allocation.is_none());
        ready_key(&mut dashboard, key(KeyCode::Enter));
        assert_eq!(new_wizard(&dashboard).step, WizardStep::Target);
    }
    replace_resource_text(&mut dashboard, "8.5");
    assert_eq!(
        new_wizard(&dashboard).resource_allocation,
        Some(SessionResourceAllocation::Container {
            cpus: 4,
            memory_bytes: (8 << 30) + (1 << 29),
        })
    );
}

#[test]
fn invalid_resource_text_survives_back_to_profile_and_reentering_target() {
    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    ready_open_new_wizard(&mut dashboard);
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Tab));
    replace_resource_text(&mut dashboard, "invalid");
    let Mode::New(wizard) = &mut dashboard.mode else {
        panic!("new session wizard");
    };
    wizard.form.get_mut().focus(WizardControl::Back);
    assert_eq!(
        new_wizard(&dashboard).form.borrow().focused(),
        Some(WizardControl::Back)
    );
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    assert_eq!(new_wizard(&dashboard).step, WizardStep::Target);
    assert_eq!(
        new_wizard(&dashboard).resource_editor.cpu.value(),
        "invalid"
    );
    assert!(new_wizard(&dashboard).resource_allocation.is_none());
}

#[test]
fn late_host_limits_preserve_typed_values_and_block_an_oversized_draft() {
    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    ready_open_new_wizard(&mut dashboard);
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Tab));
    replace_resource_text(&mut dashboard, "6");
    ready_key(&mut dashboard, key(KeyCode::Enter));
    ready_key(&mut dashboard, key(KeyCode::Enter));
    assert_eq!(new_wizard(&dashboard).step, WizardStep::Review);
    let _ = dashboard.take_prerequisite_check();
    dashboard.set_deployment_capacity_targets(vec![test_capacity_target()]);
    let sample = |cpus| {
        Ok(Some(mj_core::targets::DeploymentCapacityUsage {
            cpu_percent: None,
            memory_used_bytes: 0,
            memory_total_bytes: 64 << 30,
            logical_cores: cpus,
            disk_total_bytes: None,
            storage: Vec::new(),
        }))
    };
    dashboard.apply_deployment_capacity("local", sample(4), 0);
    set_local_container_capacity(
        &mut dashboard,
        HostContainerSize {
            cpus: 4,
            memory_bytes: 64 << 30,
        },
        HostContainerSize {
            cpus: 0,
            memory_bytes: 0,
        },
        HostContainerSize {
            cpus: 4,
            memory_bytes: (64 << 30) * 4 / 5,
        },
        true,
    );
    let wizard = new_wizard(&dashboard);
    assert_eq!(wizard.step, WizardStep::Target);
    assert_eq!(wizard.resource_editor.cpu.value(), "6");
    assert!(wizard.resource_allocation.is_none());
    assert!(
        wizard
            .sizing_error
            .as_ref()
            .unwrap()
            .contains("CPU exceeds")
    );
    assert!(!wizard.remote_preflight_in_flight);
    ready_key(&mut dashboard, key(KeyCode::Enter));
    assert_eq!(new_wizard(&dashboard).step, WizardStep::Target);
    dashboard.apply_deployment_capacity("local", sample(8), 1);
    set_local_container_capacity(
        &mut dashboard,
        HostContainerSize {
            cpus: 8,
            memory_bytes: 64 << 30,
        },
        HostContainerSize {
            cpus: 0,
            memory_bytes: 0,
        },
        HostContainerSize {
            cpus: 8,
            memory_bytes: (64 << 30) * 4 / 5,
        },
        true,
    );
    assert_eq!(new_wizard(&dashboard).resource_editor.cpu.value(), "6");
    assert_eq!(
        new_wizard(&dashboard).resource_allocation,
        Some(SessionResourceAllocation::Container {
            cpus: 6,
            memory_bytes: 8 << 30,
        })
    );
}

fn single_raw_config() -> Config {
    let mut configuration = config();
    configuration.profiles.retain(|id, _| id == "claude-1");
    configuration.targets.clear();
    configuration
        .targets
        .insert("local".into(), TargetTemplate::LocalBare);
    configuration
}

fn single_profile_and_raw_target_create_from_project_without_confirm() {
    let mut dashboard = DashboardState::new(single_raw_config(), State::default(), BTreeMap::new());
    dashboard.begin_new();
    assert_eq!(new_wizard(&dashboard).step, WizardStep::ProjectDirectory);
    let screen = captured_drawn(&mut dashboard, 100, 30).join("\n");
    assert!(screen.contains("1/1"), "{screen}");
    assert!(screen.contains("Create"), "{screen}");
    assert!(!screen.contains("Back"), "{screen}");
    if let Mode::New(wizard) = &mut dashboard.mode {
        wizard.project_directory = "/work/project".into();
    }
    assert!(matches!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::ValidateProjectDirectory { .. }
    ));
    dashboard.apply_resolved_project_directory(
        &dashboard.path_input_context(),
        "/work/project",
        Ok((
            PathBuf::from("/work/project"),
            mj_core::state::ManagedWorktreeOptions {
                available: true,
                default_create: true,
            },
            false,
        )),
    );
    assert_eq!(new_wizard(&dashboard).step, WizardStep::Launching);
    assert!(
        !captured_drawn(&mut dashboard, 100, 30)
            .join("\n")
            .contains("Confirm")
    );
    let action = dashboard.take_prerequisite_check();
    capture_detail("action", "CreateSession with managed worktree enabled");
    assert!(matches!(
        action,
        Some(DashboardAction::CreateSession {
            create_managed_worktree: Some(true),
            ..
        })
    ));
    assert!(matches!(dashboard.mode, Mode::Dashboard));
    assert!(dashboard.take_prerequisite_check().is_none());
}

fn single_profile_keeps_container_sizing_and_confirm() {
    let mut configuration = config();
    configuration.profiles.retain(|id, _| id == "claude-1");
    let mut dashboard = DashboardState::new(configuration, State::default(), BTreeMap::new());
    ready_open_new_wizard(&mut dashboard);
    assert_eq!(new_wizard(&dashboard).step, WizardStep::Target);
    let screen = captured_drawn(&mut dashboard, 120, 30).join("\n");
    assert!(screen.contains("1/3"), "{screen}");
    assert!(!screen.contains("Back"), "{screen}");
    ready_key(&mut dashboard, key(KeyCode::Enter));
    assert_eq!(new_wizard(&dashboard).step, WizardStep::Bundle);
    captured_drawn(&mut dashboard, 120, 30);
    ready_key(&mut dashboard, key(KeyCode::Enter));
    assert_eq!(new_wizard(&dashboard).step, WizardStep::Review);
    captured_drawn(&mut dashboard, 120, 30);
}

fn single_profile_raw_resume_starts_checks_without_confirm() {
    let mut dashboard = dashboard_with_session(stopped_session());
    *dashboard.config = single_raw_config();
    let session = dashboard.state.sessions.get_mut("session-1").unwrap();
    session.project_directory = Some("/work/project".into());
    session.target_template_id = "local".into();

    let action = dashboard.begin_resume_for("session-1");
    capture_detail("action", "PreflightResumeRepositories");
    assert!(
        matches!(action, DashboardAction::PreflightResumeRepositories { .. }),
        "{action:?}"
    );
    assert_eq!(resume_wizard(&dashboard).step, WizardStep::Launching);
    assert!(dashboard.resume_preflight_in_flight());
    assert!(dashboard.take_prerequisite_check().is_none());
    assert!(
        !captured_drawn(&mut dashboard, 100, 30)
            .join("\n")
            .contains("Confirm")
    );
}

#[test]
fn raw_move_keeps_review_and_session_policy_when_both_destination_pickers_are_skipped() {
    use mj_core::subagent::SubagentPolicy;
    // DSH shows a stored Native as None, which is what it does there; an
    // untouched Move still retains the stored policy.
    for (destination, stored) in [HarnessKind::Claude, HarnessKind::Dsh]
        .into_iter()
        .flat_map(|destination| {
            [
                SubagentPolicy::Native,
                SubagentPolicy::Mjolnir {
                    straightforward: None,
                    subtle: None,
                },
                SubagentPolicy::Mjolnir {
                    straightforward: Some(Box::new(mj_core::config::SubagentChoice {
                        model: "gpt-6-luna".into(),
                        effort: Some("max".into()),
                    })),
                    subtle: None,
                },
            ]
            .map(|stored| (destination, stored))
        })
    {
        let mut dashboard = dashboard_with_session(running_session());
        *dashboard.config = single_raw_config();
        dashboard.config.profiles.get_mut("claude-1").unwrap().kind = destination;
        let session = dashboard.state.sessions.get_mut("session-1").unwrap();
        session.project_directory = Some("/work/project".into());
        session.target_template_id = "local".into();
        session.subagents = Some(stored.clone());

        let DashboardAction::MoveSession {
            preparation_request_id: Some(request),
            subagents: None,
            ..
        } = dashboard.begin_move()
        else {
            panic!("prepare retains session policy");
        };
        assert_eq!(resume_wizard(&dashboard).step, WizardStep::Review);
        assert_eq!(resume_wizard(&dashboard).subagents.policy, stored);
        dashboard.take_subagent_discovery();
        assert_eq!(
            resume_wizard(&dashboard).subagents.selected(),
            stored.as_choice_for(destination)
        );
        let mut preparation = move_preparation();
        preparation.active = false;
        preparation.selection.profile_id = Some("claude-1".into());
        preparation.selection.target_template_id = Some("local".into());
        assert!(dashboard.apply_move_preparation(request, preparation));
        assert!(!matches!(
            dashboard.take_prerequisite_check(),
            Some(DashboardAction::MoveSession {
                preparation_request_id: None,
                ..
            })
        ));
        captured_drawn(&mut dashboard, 100, 30);
        assert!(matches!(
            ready_key(&mut dashboard, key(KeyCode::Enter)),
            DashboardAction::MoveSession {
                preparation_request_id: None,
                subagents: None,
                ..
            }
        ));
        assert!(!matches!(
            dashboard.take_prerequisite_check(),
            Some(DashboardAction::MoveSession {
                preparation_request_id: None,
                ..
            })
        ));
    }
}

#[test]
fn raw_resume_fast_path_failure_waits_for_explicit_retry() {
    let mut session = stopped_session();
    session.project_directory = Some("/work/project".into());
    session.target_template_id = "local".into();
    let mut dashboard = dashboard_with_session(session);
    *dashboard.config = single_raw_config();
    assert!(matches!(
        dashboard.begin_resume_for("session-1"),
        DashboardAction::PreflightResumeRepositories { .. }
    ));
    let first_generation = dashboard.session_preflight_generation();
    captured_drawn(&mut dashboard, 100, 30);
    dashboard.fail_resume_preflight("network unavailable".into());
    let screen = captured_drawn(&mut dashboard, 100, 30).join("\n");
    assert!(screen.contains("network unavailable"), "{screen}");
    assert!(screen.contains("Retry"), "{screen}");
    assert!(!screen.contains("confirm"), "{screen}");
    assert!(dashboard.take_prerequisite_check().is_none());
    assert!(matches!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::PreflightResumeRepositories { .. }
    ));
    assert_ne!(dashboard.session_preflight_generation(), first_generation);
    assert!(dashboard.resume_preflight_in_flight());
    assert!(resume_wizard(&dashboard).preflight_error.is_none());
}

#[test]
fn raw_move_keeps_file_selection_and_returns_to_review_after_it() {
    use mj_core::move_workspace::*;
    let mut session = running_session();
    session.project_directory = Some("/work/project".into());
    session.target_template_id = "local".into();
    let mut dashboard = dashboard_with_session(session);
    *dashboard.config = single_raw_config();
    let DashboardAction::MoveSession {
        preparation_request_id: Some(request),
        ..
    } = dashboard.begin_move()
    else {
        panic!("prepare")
    };
    let mut preparation = move_preparation();
    preparation.selection.profile_id = Some("claude-1".into());
    preparation.selection.target_template_id = Some("local".into());
    let mut assessment = WorkspaceAssessment::default();
    assessment.files = vec![WorkspaceFile {
        location: WorkspacePath {
            repository: "repo".into(),
            path: "large.bin".into(),
        },
        bytes: 2 << 30,
    }];
    assessment.roots = file_tree(&assessment);
    preparation.workspace = Some(assessment);
    assert!(dashboard.apply_move_preparation(request, preparation.clone()));
    assert_eq!(resume_wizard(&dashboard).step, WizardStep::MoveFiles);
    assert!(dashboard.take_prerequisite_check().is_none());
    let screen = captured_drawn(&mut dashboard, 100, 30).join("\n");
    assert!(screen.contains("× Move"), "{screen}");
    assert!(screen.contains("Back"), "{screen}");
    let DashboardAction::MoveSession {
        preparation_request_id: Some(next),
        ..
    } = ready_key(&mut dashboard, key(KeyCode::Enter))
    else {
        panic!("reprepare")
    };
    assert_ne!(request, next);
    preparation.selection.workspace = resume_wizard(&dashboard).files.selection.clone();
    assert!(preparation.selection.workspace.acknowledge_large_transfer);
    assert!(dashboard.apply_move_preparation(next, preparation));
    assert_eq!(resume_wizard(&dashboard).step, WizardStep::Review);
    assert!(dashboard.take_prerequisite_check().is_none());
    assert!(matches!(
        ready_key(&mut dashboard, key(KeyCode::Enter)),
        DashboardAction::MoveSession {
            preparation_request_id: None,
            ..
        }
    ));
    assert!(dashboard.take_prerequisite_check().is_none());
}

#[test]
fn move_combobox_mouse_selection_preserves_record_until_explicit_commit_and_reprepares() {
    use crossterm::event::{MouseButton, MouseEventKind};
    use mj_core::subagent::SubagentPolicy;
    for (width, height) in [(120, 40), (80, 32), (60, 24)] {
        let mut session = running_session();
        session.project_directory = Some("/work/project".into());
        session.target_template_id = "local".into();
        session.subagents = Some(SubagentPolicy::Mjolnir {
            straightforward: None,
            subtle: None,
        });
        let mut dashboard = dashboard_with_session(session);
        *dashboard.config = single_raw_config();
        let DashboardAction::MoveSession {
            preparation_request_id: Some(request),
            subagents: None,
            ..
        } = dashboard.begin_move()
        else {
            panic!("prepare");
        };
        let mut preparation = move_preparation();
        preparation.selection.profile_id = Some("claude-1".into());
        preparation.selection.target_template_id = Some("local".into());
        dashboard.apply_move_preparation(request, preparation);
        let Mode::Resume(wizard) = &mut dashboard.mode else {
            panic!("move");
        };
        wizard.form.get_mut().focus(WizardControl::Subagents);
        let lines = captured_drawn(&mut dashboard, width, height);
        let (label_x, y) = point(&lines, "Delegation");
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            dashboard.handle_mouse(mouse_at(kind, (label_x + 12, y)));
            captured_drawn(&mut dashboard, width, height);
        }
        assert!(
            resume_wizard(&dashboard)
                .subagents
                .combo
                .is_open(WizardControl::Subagents),
            "{width}: {lines:#?}"
        );
        dashboard.handle_key(key(KeyCode::Up));
        captured_drawn(&mut dashboard, width, height);
        dashboard.handle_key(key(KeyCode::Esc));
        assert_eq!(
            resume_wizard(&dashboard).subagents.policy,
            SubagentPolicy::Mjolnir {
                straightforward: None,
                subtle: None
            }
        );
        assert!(
            resume_wizard(&dashboard).preparation.is_some(),
            "dismissing preview keeps preparation"
        );
        let lines = captured_drawn(&mut dashboard, width, height);
        let (label_x, y) = point(&lines, "Delegation");
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            dashboard.handle_mouse(mouse_at(kind, (label_x + 12, y)));
            captured_drawn(&mut dashboard, width, height);
        }
        let lines = captured_drawn(&mut dashboard, width, height);
        let (x, y) = point(&lines, "Native");
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            dashboard.handle_mouse(mouse_at(kind, (x + 1, y)));
            captured_drawn(&mut dashboard, width, height);
        }
        assert_eq!(
            resume_wizard(&dashboard).subagents.policy,
            SubagentPolicy::Native
        );
        assert!(resume_wizard(&dashboard).preparation.is_none());
        assert_eq!(
            dashboard.state.sessions["session-1"].subagents,
            Some(SubagentPolicy::Mjolnir {
                straightforward: None,
                subtle: None
            }),
            "draft does not change recorded policy"
        );
        let Some(DashboardAction::MoveSession {
            subagents: Some(SubagentPolicy::Native),
            preparation_request_id: Some(request),
            ..
        }) = dashboard.take_prerequisite_check()
        else {
            panic!("the committed Native choice must reach preparation");
        };
        // Hard-won: 5390a50d: the displayed Native choice was sealed as an omitted override.
        let mut wrong = move_preparation();
        wrong.selection.profile_id = Some("claude-1".into());
        wrong.selection.target_template_id = Some("local".into());
        assert!(dashboard.apply_move_preparation(request, wrong.clone()));
        if let Mode::Resume(wizard) = &mut dashboard.mode {
            wizard.form.get_mut().focus(WizardControl::Submit);
        }
        let DashboardAction::MoveSession {
            subagents: Some(SubagentPolicy::Native),
            preparation_request_id: Some(retry),
            ..
        } = ready_key(&mut dashboard, key(KeyCode::Enter))
        else {
            panic!("a preparation that lost Native must be prepared again");
        };
        wrong.selection.subagents = Some(SubagentPolicy::Native);
        assert!(dashboard.apply_move_preparation(retry, wrong));
        if let Mode::Resume(wizard) = &mut dashboard.mode {
            wizard.form.get_mut().focus(WizardControl::Submit);
        }
        assert!(matches!(
            ready_key(&mut dashboard, key(KeyCode::Enter)),
            DashboardAction::MoveSession {
                subagents: Some(SubagentPolicy::Native),
                preparation_request_id: None,
                ..
            }
        ));
    }
}

#[test]
fn golden_project_picker() {
    for (label, run) in [
        (
            "project_picker_skips_empty_saved_projects_and_offers_github_from_recent",
            project_picker_skips_empty_saved_projects_and_offers_github_from_recent as fn(),
        ),
        (
            "saved_projects_show_every_source_of_the_selection_and_stack_add_and_remove",
            saved_projects_show_every_source_of_the_selection_and_stack_add_and_remove,
        ),
        (
            "project_picker_url_enter_creates_one_repository_and_advances_to_review",
            project_picker_url_enter_creates_one_repository_and_advances_to_review,
        ),
        (
            "project_picker_multiple_sources_wait_for_submit_and_remove_selected",
            project_picker_multiple_sources_wait_for_submit_and_remove_selected,
        ),
        (
            "project_picker_github_search_chooses_the_keyboard_selected_result",
            project_picker_github_search_chooses_the_keyboard_selected_result,
        ),
        (
            "project_picker_folders_navigate_up_home_and_choose_current_repository",
            project_picker_folders_navigate_up_home_and_choose_current_repository,
        ),
        (
            "project_picker_multiple_discovery_selection_toggles_members_and_can_change_primary",
            project_picker_multiple_discovery_selection_toggles_members_and_can_change_primary,
        ),
        (
            "project_picker_mouse_tabs_and_single_repository_choice_work_on_small_terminals",
            project_picker_mouse_tabs_and_single_repository_choice_work_on_small_terminals,
        ),
        (
            "project_picker_folder_choices_and_filter_remain_visible_on_small_terminals",
            project_picker_folder_choices_and_filter_remain_visible_on_small_terminals,
        ),
        (
            "project_picker_results_remain_visible_with_multiple_selected_repositories",
            project_picker_results_remain_visible_with_multiple_selected_repositories,
        ),
        (
            "project_picker_mouse_multiple_toggle_and_url_add_keep_input_above_actions",
            project_picker_mouse_multiple_toggle_and_url_add_keep_input_above_actions,
        ),
        (
            "directory_completion_lists_every_candidate_and_selects_with_keys",
            directory_completion_lists_every_candidate_and_selects_with_keys,
        ),
        (
            "clicking_a_recent_project_fills_the_field_and_enter_validates_it",
            clicking_a_recent_project_fills_the_field_and_enter_validates_it,
        ),
    ] {
        capture_scenario(label, run);
    }
    mj_core::golden::assert_golden(
        env!("CARGO_MANIFEST_DIR"),
        "project-picker",
        &take_golden_output(),
    );
}

#[test]
fn golden_profile_picker_table() {
    for (label, run) in [
        (
            "new_session_profile_step_aligns_its_columns",
            new_session_profile_step_aligns_its_columns as fn(),
        ),
        (
            "new_session_profile_step_shows_weekly_and_five_hour_percentages",
            new_session_profile_step_shows_weekly_and_five_hour_percentages,
        ),
        (
            "resume_profile_step_aligns_its_columns_and_explains_the_marker",
            resume_profile_step_aligns_its_columns_and_explains_the_marker,
        ),
        (
            "a_missing_profile_is_left_out_of_the_profile_step_and_named",
            a_missing_profile_is_left_out_of_the_profile_step_and_named,
        ),
    ] {
        capture_scenario(label, run);
    }
    mj_core::golden::assert_golden(
        env!("CARGO_MANIFEST_DIR"),
        "profile-picker-table",
        &take_golden_output(),
    );
}

fn new_session_wizard_keeps_its_frame_across_steps() {
    for (width, height) in [(140, 40), (60, 22)] {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.begin_new_wizard();
        let mut frame_bounds = None;
        for step in [
            WizardStep::Profile,
            WizardStep::Target,
            WizardStep::Bundle,
            WizardStep::NewBundle,
            WizardStep::ProjectDirectory,
            WizardStep::Review,
            WizardStep::Mounts,
            WizardStep::Launching,
        ] {
            let Mode::New(wizard) = &mut dashboard.mode else {
                unreachable!()
            };
            wizard.step = step;
            *wizard.form.borrow_mut() = Dialog::new();
            // Hard-won: ee7ee2012: wizard steps moved and resized the frame around their content.
            let lines = captured_drawn(&mut dashboard, width, height);
            let title = match step {
                WizardStep::NewBundle => "Choose a project",
                WizardStep::Mounts => "Add attached directory",
                WizardStep::Launching => "Creating session",
                _ => "New session",
            };
            let top = row_of(&lines, title);
            let left = lines[top].chars().position(|c| c == '╭').unwrap();
            let right = lines[top]
                .chars()
                .collect::<Vec<_>>()
                .iter()
                .rposition(|&c| c == '╮')
                .unwrap();
            let bottom = (top + 1..lines.len())
                .find(|&y| lines[y].chars().nth(left) == Some('╰'))
                .unwrap();
            let bounds = (left, top, right - left + 1, bottom - top + 1);
            assert_eq!(*frame_bounds.get_or_insert(bounds), bounds, "{step:?}");
            assert!(bounds.2 <= 84 && bounds.3 <= 24);
        }
    }
}

#[test]
fn golden_wizard_target_step() {
    for (label, run) in [
        (
            "new_session_wizard_keeps_its_frame_across_steps",
            new_session_wizard_keeps_its_frame_across_steps as fn(),
        ),
        (
            "new_session_prefers_interactive_choices_over_newer_unattended_sessions",
            new_session_prefers_interactive_choices_over_newer_unattended_sessions,
        ),
        (
            "new_session_skips_the_target_step_when_only_one_target_is_offered",
            new_session_skips_the_target_step_when_only_one_target_is_offered,
        ),
        (
            "a_machine_without_a_runtime_keeps_the_target_step_and_says_why",
            a_machine_without_a_runtime_keeps_the_target_step_and_says_why,
        ),
        (
            "new_session_shows_the_target_step_when_a_second_target_may_be_chosen",
            new_session_shows_the_target_step_when_a_second_target_may_be_chosen,
        ),
        (
            "a_lone_container_target_keeps_its_step_for_sizing",
            a_lone_container_target_keeps_its_step_for_sizing,
        ),
        (
            "resume_skips_the_target_step_when_only_one_target_suits_the_session",
            resume_skips_the_target_step_when_only_one_target_suits_the_session,
        ),
        (
            "resume_shows_the_target_step_when_two_targets_suit_the_session",
            resume_shows_the_target_step_when_two_targets_suit_the_session,
        ),
        (
            "move_skips_the_target_step_when_only_one_target_suits_the_session",
            move_skips_the_target_step_when_only_one_target_suits_the_session,
        ),
        (
            "move_shows_the_target_step_when_two_targets_suit_the_session",
            move_shows_the_target_step_when_two_targets_suit_the_session,
        ),
        (
            "selecting_a_row_skips_the_hidden_target",
            selecting_a_row_skips_the_hidden_target,
        ),
        (
            "target_step_draws_a_table_and_offers_resize_keys_only_for_sized_targets",
            target_step_draws_a_table_and_offers_resize_keys_only_for_sized_targets,
        ),
        (
            "container_fields_are_underlined_before_focus_and_clickable_on_small_terminals",
            container_fields_are_underlined_before_focus_and_clickable_on_small_terminals,
        ),
    ] {
        capture_scenario(label, run);
    }
    mj_core::golden::assert_golden(
        env!("CARGO_MANIFEST_DIR"),
        "wizard-target-step",
        &take_golden_output(),
    );
}

#[test]
fn golden_single_choice_fast_paths() {
    for (label, run) in [
        (
            "single_profile_and_raw_target_create_from_project_without_confirm",
            single_profile_and_raw_target_create_from_project_without_confirm as fn(),
        ),
        (
            "single_profile_keeps_container_sizing_and_confirm",
            single_profile_keeps_container_sizing_and_confirm,
        ),
        (
            "single_profile_raw_resume_starts_checks_without_confirm",
            single_profile_raw_resume_starts_checks_without_confirm,
        ),
    ] {
        capture_scenario(label, run);
    }
    mj_core::golden::assert_golden(
        env!("CARGO_MANIFEST_DIR"),
        "single-choice-fast-paths",
        &take_golden_output(),
    );
}

#[test]
fn golden_create_delegation_availability_and_submission() {
    use mj_core::config::SubagentChoice;
    use mj_core::subagent::SubagentPolicy;
    let mut output = String::new();
    for configured in [false, true] {
        let mut dashboard = dashboard_with_session(stopped_session());
        if configured {
            let choice = SubagentChoice {
                model: "model".into(),
                effort: None,
            };
            dashboard.config.subagents.straightforward = Some(choice.clone());
            dashboard.config.subagents.subtle = Some(choice);
        }
        dashboard.state.last_subagent_policy = SubagentPolicy::Mjolnir {
            straightforward: None,
            subtle: None,
        };
        dashboard.begin_new_wizard();
        let Mode::New(wizard) = &mut dashboard.mode else {
            panic!("new");
        };
        wizard.step = WizardStep::Review;
        dashboard.take_subagent_discovery();
        let Mode::New(wizard) = &mut dashboard.mode else {
            unreachable!()
        };
        assert_eq!(
            wizard.subagents.policy,
            if configured {
                SubagentPolicy::Mjolnir {
                    straightforward: None,
                    subtle: None,
                }
            } else {
                SubagentPolicy::Native
            }
        );
        wizard.subagents.select_policy(0);
        assert_eq!(wizard.subagents.select_policy(1), configured);
        let screen = drawn(&mut dashboard, 140, 40).join("\n");
        assert!(screen.contains("Delegation"));
        if !configured {
            assert!(screen.contains("Set up Mjolnir sub-agents"));
        }
        output.push_str(&format!("=== configured={configured} ===\n{screen}\n"));
        let Mode::New(wizard) = &dashboard.mode else {
            unreachable!()
        };
        let DashboardAction::CreateSession { subagents, .. } =
            dashboard.create_session_action_without_closing(wizard)
        else {
            panic!("create");
        };
        assert_eq!(
            subagents,
            Some(if configured {
                SubagentPolicy::Mjolnir {
                    straightforward: None,
                    subtle: None,
                }
            } else {
                SubagentPolicy::Native
            })
        );
        // None is available even without configured Mjolnir models.
        let Mode::New(wizard) = &mut dashboard.mode else {
            unreachable!()
        };
        wizard.form.borrow_mut().focus(WizardControl::Subagents);
        drawn(&mut dashboard, 140, 40);
        dashboard.handle_key(key(KeyCode::Enter));
        dashboard.handle_key(key(KeyCode::End));
        let screen = drawn(&mut dashboard, 140, 40).join("\n");
        output.push_str(&format!(
            "=== configured={configured}, selecting None ===\n{screen}\n"
        ));
        dashboard.handle_key(key(KeyCode::Enter));
        dashboard.take_subagent_discovery();
        let Mode::New(wizard) = &dashboard.mode else {
            unreachable!()
        };
        let action = dashboard.create_session_action_without_closing(wizard);
        assert!(
            matches!(
                &action,
                DashboardAction::CreateSession {
                    subagents: Some(SubagentPolicy::None),
                    ..
                }
            ),
            "{action:?}"
        );
        output.push_str(&format!("action: {action:?}\n"));
        // The remembered selection must survive opening another draft.
        dashboard.state.last_subagent_policy = SubagentPolicy::None;
        dashboard.begin_new_wizard();
        dashboard.take_subagent_discovery();
        assert_eq!(
            new_wizard(&dashboard).subagents.policy,
            SubagentPolicy::None
        );
    }
    // DSH has no sub-agents of its own, so Native is not offered. Create
    // starts on a remembered Mjolnir only when it is configured, and on None
    // otherwise, and submits what it shows.
    let mjolnir = SubagentPolicy::Mjolnir {
        straightforward: None,
        subtle: None,
    };
    for (remembered, configured, expected) in [
        (SubagentPolicy::Native, true, SubagentPolicy::None),
        (mjolnir.clone(), true, mjolnir.clone()),
        (mjolnir.clone(), false, SubagentPolicy::None),
    ] {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.config.profiles.get_mut("codex-1").unwrap().kind = HarnessKind::Dsh;
        if configured {
            dashboard.config.subagents.straightforward = Some(SubagentChoice {
                model: "model".into(),
                effort: None,
            });
        }
        dashboard.state.last_subagent_policy = remembered.clone();
        dashboard.begin_new_wizard();
        let dsh = dashboard
            .config
            .enabled_profiles()
            .position(|(id, _)| id == "codex-1")
            .unwrap();
        let Mode::New(wizard) = &mut dashboard.mode else {
            panic!("new");
        };
        WizardDraft::set_profile(wizard, dsh);
        wizard.step = WizardStep::Review;
        dashboard.take_subagent_discovery();
        assert_eq!(new_wizard(&dashboard).subagents.selected(), expected);
        assert_eq!(
            new_wizard(&dashboard).subagents.policies(),
            ["Mjolnir", "None"]
        );
        let Mode::New(wizard) = &mut dashboard.mode else {
            unreachable!()
        };
        wizard.form.borrow_mut().focus(WizardControl::Subagents);
        drawn(&mut dashboard, 140, 40);
        dashboard.handle_key(key(KeyCode::Enter));
        let screen = drawn(&mut dashboard, 140, 40).join("\n");
        output.push_str(&format!(
            "=== DSH, remembered {remembered:?}, configured={configured} ===\n{screen}\n"
        ));
        dashboard.handle_key(key(KeyCode::Enter));
        let Mode::New(wizard) = &dashboard.mode else {
            unreachable!()
        };
        let DashboardAction::CreateSession { subagents, .. } =
            dashboard.create_session_action_without_closing(wizard)
        else {
            panic!("create");
        };
        assert_eq!(subagents, Some(expected));
    }
    mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), "create-delegation", &output);
}

#[test]
fn move_unconfigured_delegation_retains_none_until_changed() {
    use mj_core::subagent::SubagentPolicy;
    let mut session = running_session();
    session.project_directory = Some("/work/project".into());
    session.target_template_id = "local".into();
    session.subagents = Some(SubagentPolicy::None);
    let mut dashboard = dashboard_with_session(session);
    *dashboard.config = single_raw_config();
    dashboard.begin_move();
    dashboard.take_subagent_discovery();
    let Mode::Resume(wizard) = &mut dashboard.mode else {
        panic!("move");
    };
    assert_eq!(wizard.subagents.policy, SubagentPolicy::None);
    assert!(
        wizard
            .subagents
            .unavailable_reason()
            .unwrap()
            .contains("Settings")
    );
    assert!(!wizard.subagents.select_policy(1));
    let Mode::Resume(wizard) = &dashboard.mode else {
        unreachable!()
    };
    assert!(wizard.subagent_change(&dashboard).is_none());
    assert!(
        drawn(&mut dashboard, 140, 40)
            .join("\n")
            .contains("Set up Mjolnir sub-agents")
    );
}

#[test]
fn delegation_rejects_disabled_mjolnir_on_create_and_move() {
    use mj_core::subagent::SubagentPolicy;
    let mut draft = subagents::SubagentDraft::new(SubagentPolicy::Native);
    let mut config = config();
    draft.update_choices("claude-1", &config, &Default::default());
    assert!(!draft.select_policy(1));
    assert!(draft.unavailable_reason().unwrap().contains("Settings"));
    config.profiles.get_mut("claude-1").unwrap().kind = HarnessKind::Kimi;
    draft.update_choices("claude-1", &config, &Default::default());
    assert!(!draft.select_policy(1));
    assert!(
        draft
            .unavailable_reason()
            .unwrap()
            .contains("Claude, Codex, or DSH")
    );
    // DSH lists Mjolnir and None only; unconfigured Mjolnir stays disabled.
    config.profiles.get_mut("claude-1").unwrap().kind = HarnessKind::Dsh;
    draft.update_choices("claude-1", &config, &Default::default());
    assert_eq!(draft.policies(), ["Mjolnir", "None"]);
    assert_eq!(draft.enabled_policies(), [false, true]);
    assert_eq!(draft.selected(), SubagentPolicy::None);
    assert!(!draft.select_policy(0));
    assert!(
        !draft.select_policy(1),
        "Native shown as None is already None"
    );
    assert_eq!(draft.selected(), SubagentPolicy::None);
}

// Hard-won: P2 review: changing Create's profile stranded remembered Mjolnir on an unsupported harness.
#[test]
fn create_delegation_tracks_profile_availability_in_both_directions() {
    use mj_core::config::SubagentChoice;
    use mj_core::subagent::SubagentPolicy;
    let mut dashboard = dashboard_with_session(stopped_session());
    let choice = SubagentChoice {
        model: "model".into(),
        effort: None,
    };
    dashboard.config.subagents.straightforward = Some(choice.clone());
    dashboard.config.subagents.subtle = Some(choice);
    dashboard.config.profiles.get_mut("claude-1").unwrap().kind = HarnessKind::Grok;
    dashboard.state.last_subagent_policy = SubagentPolicy::Mjolnir {
        straightforward: None,
        subtle: None,
    };
    dashboard.begin_new_wizard();
    let supported = dashboard
        .config
        .enabled_profiles()
        .position(|(id, _)| id == "codex-1")
        .unwrap();
    let unsupported = dashboard
        .config
        .enabled_profiles()
        .position(|(id, _)| id == "claude-1")
        .unwrap();
    for (profile, expected) in [
        (
            supported,
            SubagentPolicy::Mjolnir {
                straightforward: None,
                subtle: None,
            },
        ),
        (unsupported, SubagentPolicy::Native),
        (
            supported,
            SubagentPolicy::Mjolnir {
                straightforward: None,
                subtle: None,
            },
        ),
    ] {
        let Mode::New(wizard) = &mut dashboard.mode else {
            panic!("new");
        };
        WizardDraft::set_profile(wizard, profile);
        dashboard.take_subagent_discovery();
        let Mode::New(wizard) = &dashboard.mode else {
            unreachable!()
        };
        assert_eq!(wizard.subagents.policy, expected);
        assert!(wizard.subagents.error().is_none());
    }
    // Committing Native even when it is the displayed fallback is an explicit choice.
    let Mode::New(wizard) = &mut dashboard.mode else {
        unreachable!()
    };
    WizardDraft::set_profile(wizard, unsupported);
    dashboard.take_subagent_discovery();
    let Mode::New(wizard) = &mut dashboard.mode else {
        unreachable!()
    };
    wizard
        .subagents
        .apply(&Interaction::ComboBoxCommit(WizardControl::Subagents, 0));
    WizardDraft::set_profile(wizard, supported);
    dashboard.take_subagent_discovery();
    assert_eq!(
        new_wizard(&dashboard).subagents.policy,
        SubagentPolicy::Native
    );
}

// Hard-won: P2 review: Create did not reconcile delegation after global availability changed.
#[test]
fn create_delegation_tracks_config_availability_without_overwriting_explicit_choices() {
    use mj_core::config::SubagentChoice;
    use mj_core::subagent::SubagentPolicy;
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.state.last_subagent_policy = SubagentPolicy::Mjolnir {
        straightforward: None,
        subtle: None,
    };
    dashboard.begin_new_wizard();
    let supported = dashboard
        .config
        .enabled_profiles()
        .position(|(id, _)| id == "codex-1")
        .unwrap();
    let Mode::New(wizard) = &mut dashboard.mode else {
        panic!("new");
    };
    WizardDraft::set_profile(wizard, supported);
    wizard.step = WizardStep::Review;
    let choice = SubagentChoice {
        model: "model".into(),
        effort: None,
    };
    dashboard.config.subagents.straightforward = Some(choice.clone());
    dashboard.config.subagents.subtle = Some(choice.clone());
    dashboard.set_profile_capabilities(Default::default());
    assert_eq!(
        new_wizard(&dashboard).subagents.policy,
        SubagentPolicy::Mjolnir {
            straightforward: None,
            subtle: None
        }
    );
    // Subtle is optional: straightforward alone keeps Mjolnir available.
    dashboard.config.subagents.subtle = None;
    dashboard.set_profile_capabilities(Default::default());
    assert_eq!(
        new_wizard(&dashboard).subagents.policy,
        SubagentPolicy::Mjolnir {
            straightforward: None,
            subtle: None
        }
    );
    // Subtle alone is never enough.
    dashboard.config.subagents.straightforward = None;
    dashboard.config.subagents.subtle = Some(choice.clone());
    dashboard.set_profile_capabilities(Default::default());
    assert_eq!(
        new_wizard(&dashboard).subagents.policy,
        SubagentPolicy::Native
    );
    assert!(new_wizard(&dashboard).subagents.error().is_none());
    dashboard.config.subagents.straightforward = Some(choice.clone());
    dashboard.set_profile_capabilities(Default::default());
    let Mode::New(wizard) = &mut dashboard.mode else {
        unreachable!()
    };
    assert_eq!(
        wizard.subagents.policy,
        SubagentPolicy::Mjolnir {
            straightforward: None,
            subtle: None
        }
    );
    wizard
        .subagents
        .apply(&Interaction::ComboBoxCommit(WizardControl::Subagents, 1));
    dashboard.config.subagents.straightforward = None;
    dashboard.set_profile_capabilities(Default::default());
    assert_eq!(
        new_wizard(&dashboard).subagents.policy,
        SubagentPolicy::Native
    );
    dashboard.config.subagents.straightforward = Some(choice);
    dashboard.state.last_subagent_policy = SubagentPolicy::Native;
    dashboard.set_profile_capabilities(Default::default());
    assert_eq!(
        new_wizard(&dashboard).subagents.policy,
        SubagentPolicy::Mjolnir {
            straightforward: None,
            subtle: None
        }
    );
}
