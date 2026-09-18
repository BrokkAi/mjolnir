use std::collections::BTreeMap;
use std::path::Path;

use crate::targets::ProcessExecutor;
use mj_core::config::{
    Config, ContainerTemplate as ConfigContainer, HarnessKind, HarnessProfile, ProjectBundle,
    ProjectRepository, TargetTemplate,
};
use mj_core::state::State;

use super::test_support::IsolatedTest;
use super::*;

/// One profile, one bundle with nothing checked out locally, and one
/// container target, which is all `register_session_with_resources` reads.
fn registration_config() -> Config {
    let mut config = Config::default();
    config.profiles.insert(
        "codex".into(),
        HarnessProfile {
            enabled: true,
            kind: HarnessKind::Codex,
            home: PathBuf::from("/home/dev/.codex"),
            environment: BTreeMap::new(),
            context_window_bytes: None,
            guardian_review_model: None,
        },
    );
    config.bundles.insert(
        "project".into(),
        ProjectBundle {
            primary_repo: "project".into(),
            repositories: vec![ProjectRepository {
                id: "project".into(),
                github: Some("owner/project".into()),
                local: None,
                destination: PathBuf::from("project"),
                git_ref: None,
            }],
        },
    );
    config.targets.insert(
        "podman".into(),
        TargetTemplate::LocalPodman {
            container: ConfigContainer {
                build_cache: None,
                image: "example.invalid/hel-test:latest".into(),
                pull_policy: Default::default(),
                platform: None,
                cpus: None,
                memory: None,
                environment: BTreeMap::new(),
                workspace_storage: Default::default(),
            },
        },
    );
    config
}

/// A controller whose only configuration is one target template.
fn completion_controller(target_id: &str, target: &str) -> Controller {
    let target: TargetTemplate = serde_json::from_str(target).unwrap();
    let mut config = Config::default();
    config.targets.insert(target_id.into(), target);
    Controller {
        config,
        state: State::default(),
    }
}

/// Answers a remote home probe and one completion listing, and records every
/// remote command it was asked to run.
struct CompletionExecutor {
    home: &'static str,
    listing: &'static str,
    seen: std::cell::RefCell<Vec<String>>,
}

impl CompletionExecutor {
    fn new(home: &'static str, listing: &'static str) -> Self {
        Self {
            home,
            listing,
            seen: std::cell::RefCell::new(Vec::new()),
        }
    }

    fn scripts(&self) -> Vec<String> {
        self.seen.borrow().clone()
    }
}

impl CommandExecutor for CompletionExecutor {
    fn execute(&self, command: &CommandSpec) -> Result<targets::CommandOutput> {
        assert_eq!(command.program, "ssh");
        let script = command.args.last().unwrap().clone();
        self.seen.borrow_mut().push(script.clone());
        let stdout = if script.contains("$HOME") {
            self.home.as_bytes().to_vec()
        } else {
            self.listing.as_bytes().to_vec()
        };
        Ok(targets::CommandOutput {
            status: 0,
            stdout,
            stderr: Vec::new(),
        })
    }
}

/// A host that must never be asked to run anything.
struct UnusedExecutor;

impl CommandExecutor for UnusedExecutor {
    fn execute(&self, command: &CommandSpec) -> Result<targets::CommandOutput> {
        panic!("a local completion ran {}", command.program);
    }
}

#[test]
fn remote_completion_preserves_home_shorthand_and_trailing_separator() {
    let controller = completion_controller(
        "remote",
        r#"{"kind":"ssh-podman","host":"builder-shorthand","image":"test"}"#,
    );
    let executor =
        CompletionExecutor::new("/remote", "/remote/cache/alpha/\n/remote/cache/alpine/\n");

    let completion = controller
        .complete_path(
            &CompletionHost::Target("remote".into()),
            "~/cache/",
            CompletionKind::Directories,
            &executor,
        )
        .unwrap();

    assert_eq!(completion.candidates, ["~/cache/alpha/", "~/cache/alpine/"]);
    assert_eq!(completion.insert.as_deref(), Some("~/cache/alp"));
    assert!(!completion.truncated);
    assert!(
        executor.scripts()[1].contains("'/remote/cache/'"),
        "{:?}",
        executor.scripts()
    );
}

#[test]
fn bare_ssh_target_completes_over_ssh() {
    let controller = completion_controller(
        "remote",
        r#"{"kind":"ssh-bare","host":"builder-bare","permissions":"guardian"}"#,
    );
    let executor = CompletionExecutor::new("/remote", "/srv/projects/\n");

    let completion = controller
        .complete_path(
            &CompletionHost::Target("remote".into()),
            "/srv/pr",
            CompletionKind::Directories,
            &executor,
        )
        .unwrap();

    assert_eq!(completion.candidates, ["/srv/projects/"]);
    let scripts = executor.scripts();
    assert_eq!(scripts.len(), 1, "an absolute path needs no home probe");
    assert!(scripts[0].contains("ls -d --"), "{scripts:?}");
}

#[test]
fn local_bare_and_ec2_targets_complete_on_the_controller() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("projects")).unwrap();
    let prefix = format!("{}/pro", directory.path().display());
    let expected = vec![format!("{}/projects/", directory.path().display())];

    for target in [
        r#"{"kind":"local-bare"}"#,
        r#"{"kind":"aws-ec2","region":"us-east-1","launch_template":"lt-1","ssh_user":"ubuntu"}"#,
    ] {
        let controller = completion_controller("host", target);
        let completion = controller
            .complete_path(
                &CompletionHost::Target("host".into()),
                &prefix,
                CompletionKind::Directories,
                &UnusedExecutor,
            )
            .unwrap();
        assert_eq!(completion.candidates, expected, "{target}");
    }
}

#[test]
fn machine_host_completes_files_for_any_kind() {
    let machine: mj_core::config::Machine =
        serde_json::from_str(r#"{"kind":"ssh","host":"builder-machine"}"#).unwrap();
    let controller = Controller {
        config: Config::default(),
        state: State::default(),
    };
    let executor = CompletionExecutor::new("/remote", "/srv/keys/\n/srv/key.pub\n");

    let completion = controller
        .complete_path(
            &CompletionHost::Machine(Box::new(machine)),
            "/srv/key",
            CompletionKind::Any,
            &executor,
        )
        .unwrap();

    assert_eq!(completion.candidates, ["/srv/key.pub", "/srv/keys/"]);
    assert!(
        executor.scripts()[0].contains("ls -dp --"),
        "{:?}",
        executor.scripts()
    );
}

#[test]
fn remote_home_is_probed_once_per_host() {
    let controller = completion_controller(
        "remote",
        r#"{"kind":"ssh-bare","host":"builder-home-once","permissions":"guardian"}"#,
    );
    let executor = CompletionExecutor::new("/remote", "/remote/cache/alpha/\n");

    for _ in 0..2 {
        controller
            .complete_path(
                &CompletionHost::Target("remote".into()),
                "~/cache/",
                CompletionKind::Directories,
                &executor,
            )
            .unwrap();
    }

    let probes = executor
        .scripts()
        .iter()
        .filter(|script| script.contains("$HOME"))
        .count();
    assert_eq!(probes, 1, "{:?}", executor.scripts());
}

#[test]
fn more_than_fifty_matches_are_truncated() {
    let directory = tempfile::tempdir().unwrap();
    for index in 0..60 {
        std::fs::create_dir(directory.path().join(format!("project-{index:03}"))).unwrap();
    }
    let controller = completion_controller("host", r#"{"kind":"local-bare"}"#);

    let completion = controller
        .complete_path(
            &CompletionHost::Target("host".into()),
            &format!("{}/pro", directory.path().display()),
            CompletionKind::Directories,
            &UnusedExecutor,
        )
        .unwrap();

    assert_eq!(completion.candidates.len(), 50);
    assert!(completion.truncated);
    assert_eq!(
        completion.insert.as_deref(),
        Some(format!("{}/project-0", directory.path().display()).as_str())
    );
}

#[test]
fn remote_path_resolution_uses_login_home_without_evaluating_suffix() {
    struct HomeExecutor {
        status: i32,
        home: &'static str,
    }
    impl CommandExecutor for HomeExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<targets::CommandOutput> {
            assert_eq!(command.program, "ssh");
            let script = command.args.last().unwrap();
            assert!(!script.contains("touch"));
            assert!(script.contains("$HOME"));
            Ok(targets::CommandOutput {
                status: self.status,
                stdout: self.home.as_bytes().to_vec(),
                stderr: b"home lookup failed".to_vec(),
            })
        }
    }
    // A resolved home is cached per host, so each case names its own host.
    fn target(host: &str) -> TargetTemplate {
        serde_json::from_str(&format!(
            r#"{{"kind":"ssh-bare","host":"{host}","permissions":"guardian"}}"#
        ))
        .unwrap()
    }
    let path = Path::new("~/資料/$(touch nope)");
    assert_eq!(
        resolve_target_input_path(
            &target("builder-home-ok"),
            path,
            &HomeExecutor {
                status: 0,
                home: "/remote user"
            }
        )
        .unwrap(),
        Path::new("/remote user/資料/$(touch nope)")
    );
    assert!(
        resolve_target_input_path(
            &target("builder-home-failed"),
            path,
            &HomeExecutor {
                status: 1,
                home: "/remote"
            }
        )
        .unwrap_err()
        .to_string()
        .contains("home lookup failed")
    );
    assert!(
        resolve_target_input_path(
            &target("builder-home-empty"),
            path,
            &HomeExecutor {
                status: 0,
                home: ""
            }
        )
        .is_err()
    );
    assert!(
        resolve_target_input_path(
            &target("builder-home-relative"),
            path,
            &HomeExecutor {
                status: 0,
                home: "relative"
            }
        )
        .is_err()
    );
}

#[test]
fn bundle_creation_combines_sources_with_first_primary_and_stable_collisions() {
    let mut config = Config::default();
    let sources = vec!["example/app".into(), "other/app".into()];

    let bundle_id = create_bundle_from_sources_in_config(&mut config, &sources).unwrap();
    let bundle = &config.bundles[&bundle_id];
    assert_eq!(bundle_id, "app");
    assert_eq!(bundle.primary_repo, "app");
    assert_eq!(
        bundle
            .repositories
            .iter()
            .map(|repository| repository.id.as_str())
            .collect::<Vec<_>>(),
        ["app", "app-2"]
    );
    assert_eq!(
        bundle
            .repositories
            .iter()
            .map(|repository| repository.destination.to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        ["app".to_owned(), "app-2".to_owned()]
    );
    assert_eq!(
        bundle.repositories[0].github.as_deref(),
        Some("example/app")
    );
    assert_eq!(bundle.repositories[1].github.as_deref(), Some("other/app"));
}

#[test]
fn bundle_creation_combines_local_and_github_sources_and_rejects_local_aliases() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("app");
    let output = mj_core::subprocess::run_capturing_stdout(
        std::process::Command::new("git").arg("init").arg(&root),
    )
    .unwrap();
    assert!(output.status.success(), "{output:?}");
    let nested = root.join("nested");
    fs::create_dir(&nested).unwrap();
    let mut config = Config::default();
    let sources = vec![root.to_str().unwrap().into(), "example/shared".into()];
    let id = create_bundle_from_sources_in_config(&mut config, &sources).unwrap();
    let bundle = &config.bundles[&id];
    assert_eq!(
        bundle.primary().unwrap().local,
        Some(root.canonicalize().unwrap())
    );
    assert_eq!(
        bundle.repositories[1].github.as_deref(),
        Some("example/shared")
    );
    let before = config.clone();
    let aliases = vec![
        root.to_str().unwrap().into(),
        nested.to_str().unwrap().into(),
    ];
    let error = create_bundle_from_sources_in_config(&mut config, &aliases).unwrap_err();
    assert!(
        error.to_string().contains("duplicate repository source"),
        "{error:#}"
    );
    assert_eq!(config, before);
}

#[test]
fn bundle_creation_rejects_duplicate_normalized_sources_atomically() {
    let mut config = Config::default();
    let before = config.clone();
    let sources = vec![
        "example/app".into(),
        "https://github.com/EXAMPLE/APP.git".into(),
    ];

    let error = create_bundle_from_sources_in_config(&mut config, &sources).unwrap_err();
    assert!(error.to_string().contains("duplicate repository source"));
    assert_eq!(config, before);
}

#[test]
fn bundle_creation_validates_every_source_before_mutating_config() {
    let mut config = Config::default();
    let before = config.clone();
    let invalid_directory = tempfile::tempdir().unwrap();
    let sources = vec![
        "example/app".into(),
        invalid_directory.path().to_string_lossy().into_owned(),
    ];

    let error = create_bundle_from_sources_in_config(&mut config, &sources).unwrap_err();
    assert!(error.to_string().contains("not a Git repository"));
    assert_eq!(config, before);
}

#[test]
fn bundle_creation_reuses_an_exact_source_set_and_rejects_obsolete_pins() {
    let mut config = Config::default();
    config.bundles.insert(
        "all".into(),
        ProjectBundle {
            primary_repo: "app".into(),
            repositories: vec![
                ProjectRepository {
                    id: "app".into(),
                    github: Some("example/app".into()),
                    local: None,
                    destination: "app".into(),
                    git_ref: None,
                },
                ProjectRepository {
                    id: "shared".into(),
                    github: Some("example/shared".into()),
                    local: None,
                    destination: "shared".into(),
                    git_ref: None,
                },
            ],
        },
    );

    let one_source = vec!["example/app".into()];
    let created = create_bundle_from_sources_in_config(&mut config, &one_source).unwrap();
    assert_eq!(created, "app");
    assert_eq!(config.bundles[&created].repositories.len(), 1);
    assert_eq!(
        create_bundle_from_sources_in_config(&mut config, &one_source).unwrap(),
        "app"
    );

    let exact_sources = vec!["example/app".into(), "example/shared".into()];
    assert_eq!(
        create_bundle_from_sources_in_config(&mut config, &exact_sources).unwrap(),
        "all"
    );
    assert_eq!(config.bundles.len(), 2);
    config.bundles.get_mut("all").unwrap().repositories[0].git_ref = Some("release".into());
    let before = config.clone();
    let error = create_bundle_from_sources_in_config(&mut config, &exact_sources).unwrap_err();
    assert!(format!("{error:#}").contains("git_ref is no longer supported"));
    assert_eq!(config, before);
}

fn launch_options(additional_mounts: Vec<AdditionalMount>) -> SessionLaunchOptions {
    SessionLaunchOptions {
        mjolnir_subagents: None,
        create_managed_worktree: None,
        initial_prompt: None,
        workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned(),
        additional_mounts,
        resource_allocation: None,
        project_directory: None,
        session_title_override: None,
    }
}

#[test]
fn registration_rejects_a_disabled_profile_before_persisting() {
    let mut config = registration_config();
    config.profiles.get_mut("codex").unwrap().enabled = false;
    let mut controller = Controller {
        config,
        state: State::default(),
    };

    let error = controller
        .register_session_with_resources(
            "codex",
            "project",
            "podman",
            "disabled",
            launch_options(Vec::new()),
        )
        .unwrap_err();

    assert!(error.to_string().contains("disabled"));
    assert!(controller.state.sessions.is_empty());
}

#[test]
fn muse_registration_rejects_more_than_one_workspace_root_before_persisting() {
    let mut config = registration_config();
    config.profiles.get_mut("codex").unwrap().kind = HarnessKind::Muse;
    let second = config.bundles["project"].repositories[0].clone();
    config
        .bundles
        .get_mut("project")
        .unwrap()
        .repositories
        .push(mj_core::config::ProjectRepository {
            id: "second".into(),
            destination: "second".into(),
            ..second
        });
    let mut controller = Controller {
        config,
        state: State::default(),
    };

    let error = controller
        .register_session_with_resources(
            "codex",
            "project",
            "podman",
            "unsupported",
            launch_options(Vec::new()),
        )
        .unwrap_err();

    assert!(error.to_string().contains("one workspace root"));
    assert!(controller.state.sessions.is_empty());
}

/// MJ_DATA_DIR is process-global, so every test that reaches the
/// controller database runs in an exact child with its own data directory.
fn run_registration_child(marker: &str, test: &str, data_directory: &Path) {
    IsolatedTest::new(format!("controller::tests::{test}"))
        .env(marker, "1")
        .env("MJ_DATA_DIR", data_directory)
        .env("MJ_CONFIG_DIR", data_directory)
        .run();
}

#[test]
fn registration_saves_the_initial_task_before_provisioning() {
    const MARKER: &str = "MJ_TEST_INITIAL_TASK_CHILD";
    if std::env::var_os(MARKER).is_none() {
        let directory = tempfile::tempdir().unwrap();
        run_registration_child(
            MARKER,
            "registration_saves_the_initial_task_before_provisioning",
            directory.path(),
        );
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let mut controller = Controller {
        config: registration_config(),
        state: State::default(),
    };
    let prompt = format!(
        "Initial task\n{}\n\tPreserve indentation and λ",
        "x".repeat(70_000)
    );
    let mut options = launch_options(Vec::new());
    options.initial_prompt = Some(prompt.clone());
    let id = controller
        .register_session_with_resources("codex", "project", "podman", "fresh task", options)
        .unwrap();
    let saved = crate::database::load_state().unwrap();
    assert_eq!(saved.sessions[&id].draft_input, prompt);
    assert_eq!(saved.sessions[&id].state, SessionState::Provisioning);
    crate::database::set_session_draft_input(&id, "a newer draft").unwrap();
    crate::database::clear_session_draft_input_if_matches(&id, &prompt).unwrap();
    assert_eq!(
        crate::database::load_state().unwrap().sessions[&id].draft_input,
        "a newer draft"
    );
    crate::database::clear_session_draft_input_if_matches(&id, "a newer draft").unwrap();
    assert!(
        crate::database::load_state().unwrap().sessions[&id]
            .draft_input
            .is_empty()
    );
}

const UNPERSISTABLE_SESSION_CHILD: &str = "MJ_TEST_UNPERSISTABLE_SESSION_CHILD";

#[test]
fn missing_bundle_does_not_block_controller_or_other_sessions() {
    const CHILD: &str = "MJ_TEST_MISSING_BUNDLE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        run_registration_child(
            CHILD,
            "missing_bundle_does_not_block_controller_or_other_sessions",
            directory.path(),
        );
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let mut controller = Controller {
        config: registration_config(),
        state: State::default(),
    };
    let bundle = controller.config.bundles["project"].clone();
    controller
        .config
        .bundles
        .insert("other".into(), bundle.clone());
    controller.config.save().unwrap();
    let affected = controller
        .register_session_with_resources(
            "codex",
            "project",
            "podman",
            "affected",
            launch_options(Vec::new()),
        )
        .unwrap();
    let healthy = controller
        .register_session_with_resources(
            "codex",
            "other",
            "podman",
            "healthy",
            launch_options(Vec::new()),
        )
        .unwrap();
    controller.config.bundles.remove("project");
    controller.config.save().unwrap();
    let loaded = Controller::load().unwrap();
    assert_eq!(loaded.state.sessions.len(), 2);
    assert!(
        loaded.state.sessions[&healthy]
            .configuration_issue(&loaded.config)
            .is_none()
    );
    let issue = loaded.reconnect_command(&affected).unwrap_err().to_string();
    assert!(issue.contains("missing bundle"), "{issue}");
    assert!(issue.contains("config.toml"), "{issue}");
    // Loading must not turn a configuration problem into a persisted lifecycle failure.
    assert_eq!(
        loaded.state.sessions[&affected],
        controller.state.sessions[&affected]
    );
    Config::update(|config| {
        config.bundles.insert("project".into(), bundle);
        Ok(())
    })
    .unwrap();
    let repaired = Controller::load().unwrap();
    assert!(
        repaired.state.sessions[&affected]
            .configuration_issue(&repaired.config)
            .is_none()
    );
}

const CONFIG_ID_RENAME_CHILD: &str = "MJ_TEST_CONFIG_ID_RENAME_CHILD";

#[test]
fn configuration_id_rename_rewrites_durable_session_references() {
    if std::env::var_os(CONFIG_ID_RENAME_CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        run_registration_child(
            CONFIG_ID_RENAME_CHILD,
            "configuration_id_rename_rewrites_durable_session_references",
            directory.path(),
        );
        return;
    }
    // Alone in this child process, so it installs the one writer.
    let _writer = crate::database::install_isolated_test_writer();

    let mut controller = Controller {
        config: registration_config(),
        state: State::default(),
    };
    controller.config.save().unwrap();
    let session_id = controller
        .register_session_with_resources(
            "codex",
            "project",
            "podman",
            "rename references",
            launch_options(Vec::new()),
        )
        .unwrap();

    controller
        .rename_profile_id("codex", "codex-renamed")
        .unwrap();
    controller
        .rename_target_id("podman", "podman-renamed")
        .unwrap();

    let loaded = Controller::load().unwrap();
    let session = &loaded.state.sessions[&session_id];
    assert_eq!(session.last_profile, "codex-renamed");
    assert_eq!(session.target_template_id, "podman-renamed");
    assert!(loaded.config.profiles.contains_key("codex-renamed"));
    assert!(loaded.config.targets.contains_key("podman-renamed"));
    assert!(!config_rename_journal_path().exists());
}

#[test]
fn a_session_the_database_rejects_is_never_left_in_memory() {
    if std::env::var_os(UNPERSISTABLE_SESSION_CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        run_registration_child(
            UNPERSISTABLE_SESSION_CHILD,
            "a_session_the_database_rejects_is_never_left_in_memory",
            directory.path(),
        );
        return;
    }
    // Alone in this child process, so it installs the one writer.
    let _writer = crate::database::install_isolated_test_writer();

    let mut controller = Controller {
        config: registration_config(),
        state: State::default(),
    };
    // The store has to be healthy enough to open before it can reject a
    // write: this test is about a write the database refuses, not about a
    // store that cannot be opened at all, which now fails earlier and
    // louder when the writer is installed. The first registration builds
    // the schema the second one then loses.
    controller
        .register_session_with_resources(
            "codex",
            "project",
            "podman",
            "first",
            launch_options(Vec::new()),
        )
        .expect("a healthy store registers a session");
    rusqlite::Connection::open(crate::database::database_path())
        .unwrap()
        .execute_batch("DROP TABLE sessions")
        .unwrap();

    let error = controller
        .register_session_with_resources(
            "codex",
            "project",
            "podman",
            "unpersistable",
            launch_options(Vec::new()),
        )
        .expect_err("a store that rejects the write cannot register a session");
    assert!(
        format!("{error:#}").contains("sessions"),
        "unexpected error: {error:#}"
    );
    assert_eq!(
        controller.state.sessions.len(),
        1,
        "a session the database never accepted stayed in controller memory"
    );
    assert!(
        controller
            .state
            .sessions
            .values()
            .all(|session| session.title != "unpersistable"),
        "the rejected session is the one that stayed"
    );
}

const MOUNT_HISTORY_FAILURE_CHILD: &str = "MJ_TEST_MOUNT_HISTORY_FAILURE_CHILD";

const CONTAINER_SIZE_HISTORY_CHILD: &str = "MJ_TEST_CONTAINER_SIZE_HISTORY_CHILD";

#[test]
fn registration_remembers_launch_size_but_session_overrides_do_not_replace_it() {
    if std::env::var_os(CONTAINER_SIZE_HISTORY_CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        run_registration_child(
            CONTAINER_SIZE_HISTORY_CHILD,
            "registration_remembers_launch_size_but_session_overrides_do_not_replace_it",
            directory.path(),
        );
        return;
    }
    // Alone in this child process, so it installs the one writer.
    let _writer = crate::database::install_isolated_test_writer();

    let mut controller = Controller {
        config: registration_config(),
        state: State::default(),
    };
    let mut options = launch_options(Vec::new());
    options.resource_allocation = Some(SessionResourceAllocation::Container {
        cpus: 12,
        memory_bytes: 48 * 1024 * 1024 * 1024,
    });
    let id = controller
        .register_session_with_resources("codex", "project", "podman", "sized", options)
        .unwrap();
    let expected = HostContainerSize {
        cpus: 12,
        memory_bytes: 48 * 1024 * 1024 * 1024,
    };
    assert_eq!(controller.state.container_sizes["local"], expected);
    assert_eq!(
        crate::database::load_state().unwrap().container_sizes["local"],
        expected
    );

    controller
        .update_session_container_settings(
            &id,
            Some("2".into()),
            Some("4g".into()),
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
    assert_eq!(
        crate::database::load_state().unwrap().container_sizes["local"],
        expected
    );
}

#[test]
fn a_failed_mount_history_write_does_not_fail_the_registered_session() {
    if std::env::var_os(MOUNT_HISTORY_FAILURE_CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        run_registration_child(
            MOUNT_HISTORY_FAILURE_CHILD,
            "a_failed_mount_history_write_does_not_fail_the_registered_session",
            directory.path(),
        );
        return;
    }
    // Alone in this child process, so it installs the one writer.
    let _writer = crate::database::install_isolated_test_writer();

    let mut controller = Controller {
        config: registration_config(),
        state: State::default(),
    };
    // The first registration builds the schema this test then breaks.
    controller
        .register_session_with_resources(
            "codex",
            "project",
            "podman",
            "first",
            launch_options(Vec::new()),
        )
        .expect("a healthy store registers a session");
    let database = crate::database::database_path();
    rusqlite::Connection::open(&database)
        .unwrap()
        .execute_batch("DROP TABLE mount_history")
        .unwrap();

    let id = controller
        .register_session_with_resources(
            "codex",
            "project",
            "podman",
            "attached",
            launch_options(vec![AdditionalMount {
                source: PathBuf::from("/host/models"),
                destination: PathBuf::from("/mnt/models"),
                access: crate::targets::MountAccess::Cow,
            }]),
        )
        .expect("a suggestion list that cannot be written must not fail a registration");

    let stored: i64 = rusqlite::Connection::open(&database)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM sessions WHERE session_id = ?1",
            [&id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored, 1, "the registered session was not committed");
    assert!(
        controller.state.mount_history.is_empty(),
        "controller memory remembered mount sources the database never stored"
    );
}

#[test]
fn command_errors_report_the_root_cause_without_worker_wrappers() {
    let stderr = b"Error: restore target checkpoint failed with status 1: Error: restore repository \"bifrost\"\n\nCaused by:\n    checkpoint base b41dc78 is absent from configured source\n    repository may have moved\n";

    assert_eq!(
        command_error_detail(stderr),
        "checkpoint base b41dc78 is absent from configured source\nrepository may have moved"
    );
}

#[test]
fn controller_store_lock_excludes_a_second_process_owner() {
    let directory = tempfile::tempdir().unwrap();
    let first = ControllerStoreGuard::acquire_at(directory.path()).unwrap();
    run_controller_lock_probe(directory.path(), true);
    drop(first);
    run_controller_lock_probe(directory.path(), false);
}
fn run_controller_lock_probe(directory: &Path, expect_locked: bool) {
    IsolatedTest::new("controller::tests::controller_store_lock_subprocess_probe")
        .env("MJ_CONTROLLER_LOCK_PROBE", directory)
        .env(
            "MJ_CONTROLLER_LOCK_EXPECTED",
            if expect_locked { "locked" } else { "available" },
        )
        .run();
}
#[test]
fn controller_store_lock_subprocess_probe() {
    let Some(directory) = std::env::var_os("MJ_CONTROLLER_LOCK_PROBE") else {
        return;
    };
    let expected = std::env::var("MJ_CONTROLLER_LOCK_EXPECTED").unwrap();
    let acquired = ControllerStoreGuard::acquire_at(Path::new(&directory));
    match expected.as_str() {
        "locked" => {
            let error = acquired.expect_err("a second process acquired the controller store");
            assert!(error.to_string().contains("another Mjolnir controller"));
        }
        "available" => {
            acquired.expect("released controller store stayed locked");
        }
        value => panic!("unexpected lock probe expectation {value:?}"),
    }
}
#[test]
fn local_mount_source_must_be_an_existing_directory() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("file");
    std::fs::write(&file, "not a directory").unwrap();
    let mut config = Config::default();
    config.targets.insert(
        "local".into(),
        TargetTemplate::LocalPodman {
            container: ConfigContainer {
                build_cache: None,
                image: "ubuntu:24.04".into(),
                pull_policy: Default::default(),
                platform: None,
                cpus: None,
                memory: None,
                environment: BTreeMap::new(),
                workspace_storage: Default::default(),
            },
        },
    );
    let controller = Controller {
        config,
        state: State::default(),
    };

    assert!(
        controller
            .validate_mount_source("local", directory.path(), &ProcessExecutor)
            .is_ok()
    );
    for invalid in [file, directory.path().join("missing")] {
        let error = controller
            .validate_mount_source("local", &invalid, &ProcessExecutor)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not exist or is not a directory")
        );
    }
}

/// A relaunch rewrites `launch.json`. It must carry the session's delegation
/// choice, or the new worker never serves the sub-agent socket (#1067).
#[test]
fn a_relaunch_config_keeps_the_sessions_subagent_tools() {
    const MARKER: &str = "MJ_TEST_RELAUNCH_SUBAGENT_TOOLS_CHILD";
    if std::env::var_os(MARKER).is_none() {
        let directory = tempfile::tempdir().unwrap();
        run_registration_child(
            MARKER,
            "a_relaunch_config_keeps_the_sessions_subagent_tools",
            directory.path(),
        );
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let mut controller = Controller {
        config: registration_config(),
        state: State::default(),
    };
    let mut options = launch_options(Vec::new());
    options.mjolnir_subagents = Some(true);
    let id = controller
        .register_session_with_resources("codex", "project", "podman", "delegating", options)
        .unwrap();
    let backend = crate::targets::TargetLocator::LocalPodman {
        borrowed_from: None,
        container_id: "container".into(),
        workspace_storage: Default::default(),
    };

    let relaunch = controller
        .current_worker_launch_config(&id, &backend)
        .unwrap();

    assert!(relaunch.subagent_tools);
}

/// Capturing the working tree is only ever useful to a turn review, so it is
/// armed only when the configuration names a reviewer. A session started with
/// no reviewer must do no Git work at startup at all (#1065).
#[test]
fn a_launch_config_arms_the_review_capture_only_when_a_reviewer_is_configured() {
    const MARKER: &str = "MJ_TEST_LAUNCH_REVIEW_CAPTURE_CHILD";
    if std::env::var_os(MARKER).is_none() {
        let directory = tempfile::tempdir().unwrap();
        run_registration_child(
            MARKER,
            "a_launch_config_arms_the_review_capture_only_when_a_reviewer_is_configured",
            directory.path(),
        );
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let mut controller = Controller {
        config: registration_config(),
        state: State::default(),
    };
    let id = controller
        .register_session_with_resources(
            "codex",
            "project",
            "podman",
            "reviewable",
            launch_options(Vec::new()),
        )
        .unwrap();
    let backend = crate::targets::TargetLocator::LocalPodman {
        borrowed_from: None,
        container_id: "container".into(),
        workspace_storage: Default::default(),
    };

    assert!(
        !controller
            .current_worker_launch_config(&id, &backend)
            .unwrap()
            .review_capture,
        "with no [review] profile there is nothing a capture could serve"
    );

    controller.config.review.profile = Some("codex".into());

    assert!(
        controller
            .current_worker_launch_config(&id, &backend)
            .unwrap()
            .review_capture,
        "a session a review can run for takes a baseline"
    );
}
