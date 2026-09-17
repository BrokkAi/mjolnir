use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;

use crate::controller::Controller;
use crate::controller::resume::apply_failed_resume_rollback;
use crate::controller::test_support::{
    FIXTURE_FETCH_URL, FixtureRemoteExecutor, IsolatedTest, checkout_with_network_remote,
    checkpoint_test_session, committed_repository, local_bundle, managed_raw_session,
    managed_worktree_session, raw_session_on, resume_compatibility_config, ssh_worktree_target,
    test_git, test_name,
};
use mj_checkpoint::archive::RepositoryMetadata;
use mj_core::config::{Config, HarnessProfile, ProjectBundle, ProjectRepository, TargetTemplate};
use mj_core::state::{ManagedWorktree, ManagedWorktreeTarget, SessionState, State};

use crate::targets::{
    CancellableProcessExecutor, CommandExecutor, CommandOutput, CommandSpec, ProcessExecutor,
};

use super::*;

#[test]
fn worktree_choice_survives_reload_and_controls_creation() {
    const CHILD: &str = "MJ_TEST_WORKTREE_CHOICE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(test_name(
            module_path!(),
            "worktree_choice_survives_reload_and_controls_creation",
        ))
        .env(CHILD, "1")
        .env("MJ_DATA_DIR", directory.path())
        .env("MJ_CONFIG_DIR", directory.path())
        .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let repository = committed_repository();
    let root = repository.path().canonicalize().unwrap();
    let linked_parent = tempfile::tempdir().unwrap();
    let linked = linked_parent.path().join("linked");
    test_git(
        &root,
        &["worktree", "add", "-b", "side", linked.to_str().unwrap()],
    );
    test_git(&linked, &["branch", "--set-upstream-to=master"]);
    std::fs::write(linked.join("nested/file.txt"), "linked commit\n").unwrap();
    test_git(&linked, &["commit", "-am", "side commit"]);
    let linked = linked.canonicalize().unwrap();
    let mut config = Config::default();
    config
        .targets
        .insert("localhost".into(), TargetTemplate::LocalBare);
    config.save().unwrap();
    let mut controller = Controller {
        config,
        state: State::default(),
    };
    assert_eq!(
        controller
            .managed_worktree_options("localhost", &root, &ProcessExecutor)
            .unwrap(),
        ManagedWorktreeOptions {
            available: true,
            default_create: true
        }
    );
    assert_eq!(
        controller
            .managed_worktree_options("localhost", &linked, &ProcessExecutor)
            .unwrap(),
        ManagedWorktreeOptions {
            available: true,
            default_create: false
        }
    );

    // Both creation and first resume of an imported session enter this preparation.
    // A dirty source is usable directly, and cleanup must leave its files and branch alone.
    std::fs::write(root.join("dirty.txt"), "keep me\n").unwrap();
    let selected = root.join("nested");
    let mut record = raw_session_on("localhost", selected.to_str().unwrap());
    record.create_managed_worktree = Some(false);
    crate::database::save_session(&record).unwrap();
    for _ in 0..2 {
        controller.reload().unwrap();
        assert!(
            !controller
                .prepare_managed_raw_worktree(&record.id, &ProcessExecutor)
                .unwrap()
        );
        assert_eq!(
            controller.state.sessions[&record.id]
                .project_directory
                .as_ref(),
            Some(&selected)
        );
        assert!(
            controller.state.sessions[&record.id]
                .managed_worktree
                .is_none()
        );
        controller
            .cleanup_new_session_worktree(&record.id, &ProcessExecutor)
            .unwrap();
    }
    assert!(root.join("dirty.txt").exists());
    assert!(!root.join(".mj/worktrees").exists());
    assert_eq!(test_git(&root, &["branch", "--show-current"]), "master");
    std::fs::remove_file(root.join("dirty.txt")).unwrap();

    // Explicit creation also works from a linked checkout and preserves its HEAD,
    // upstream, and selected subdirectory rather than using the main checkout's HEAD.
    record.project_directory = Some(linked.join("nested"));
    record.create_managed_worktree = None;
    crate::database::save_session(&record).unwrap();
    controller.reload().unwrap();
    assert!(
        !controller
            .prepare_managed_raw_worktree(&record.id, &ProcessExecutor)
            .unwrap()
    );
    record.create_managed_worktree = Some(true);
    crate::database::save_session(&record).unwrap();
    controller.reload().unwrap();
    assert!(
        controller
            .prepare_managed_raw_worktree(&record.id, &ProcessExecutor)
            .unwrap()
    );
    controller.reload().unwrap();
    let managed = controller.state.sessions[&record.id]
        .managed_worktree
        .clone()
        .unwrap();
    assert_eq!(
        test_git(&managed.worktree_root, &["rev-parse", "HEAD"]),
        test_git(&linked, &["rev-parse", "HEAD"])
    );
    assert_ne!(
        test_git(&managed.worktree_root, &["rev-parse", "HEAD"]),
        test_git(&root, &["rev-parse", "HEAD"])
    );
    assert_eq!(
        test_git(
            &managed.worktree_root,
            &["rev-parse", "--abbrev-ref", "@{upstream}"]
        ),
        "master"
    );
    assert_eq!(
        controller.state.sessions[&record.id].project_directory,
        Some(managed.worktree_root.join("nested"))
    );
    controller
        .cleanup_new_session_worktree(&record.id, &ProcessExecutor)
        .unwrap();
    assert!(linked.join("nested/file.txt").exists());
    assert!(!managed.worktree_root.exists());

    record.project_directory = Some(root.clone());
    record.create_managed_worktree = None;
    crate::database::save_session(&record).unwrap();
    controller.reload().unwrap();
    assert!(
        controller
            .prepare_managed_raw_worktree(&record.id, &ProcessExecutor)
            .unwrap()
    );
    controller
        .cleanup_new_session_worktree(&record.id, &ProcessExecutor)
        .unwrap();
}

#[test]
fn explicit_worktree_creation_rejects_plain_directories() {
    let directory = tempfile::tempdir().unwrap();
    let mut record = raw_session_on("localhost", directory.path().to_str().unwrap());
    record.create_managed_worktree = Some(true);
    let id = record.id.clone();
    let mut config = Config::default();
    config
        .targets
        .insert("localhost".into(), TargetTemplate::LocalBare);
    let mut controller = Controller {
        config,
        state: State {
            sessions: [(id.clone(), record)].into_iter().collect(),
            ..State::default()
        },
    };
    assert_eq!(
        controller
            .managed_worktree_options("localhost", directory.path(), &ProcessExecutor)
            .unwrap(),
        ManagedWorktreeOptions::default()
    );
    let error = controller
        .prepare_managed_raw_worktree(&id, &ProcessExecutor)
        .unwrap_err();
    assert!(error.to_string().contains("requires a Git project"));
    assert!(!directory.path().join(".git").exists());
}

#[test]
fn local_bare_validation_accepts_projects_and_plain_directories_but_rejects_missing_paths() {
    let project = committed_repository();
    let plain = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config
        .targets
        .insert("localhost".into(), TargetTemplate::LocalBare);
    let controller = Controller {
        config,
        state: State::default(),
    };
    controller
        .validate_project_directory("localhost", project.path(), &ProcessExecutor)
        .unwrap();
    controller
        .validate_project_directory("localhost", plain.path(), &ProcessExecutor)
        .unwrap();
    assert!(
        controller
            .validate_project_directory(
                "localhost",
                &plain.path().join("missing"),
                &ProcessExecutor
            )
            .is_err()
    );
}

#[test]
fn a_plain_local_directory_starts_without_creating_a_git_worktree() {
    let plain = tempfile::tempdir().unwrap();
    let session = raw_session_on("localhost", plain.path().to_str().unwrap());
    let session_id = session.id.clone();
    let mut config = Config::default();
    config
        .targets
        .insert("localhost".into(), TargetTemplate::LocalBare);
    let mut controller = Controller {
        config,
        state: State {
            sessions: [(session_id.clone(), session)].into_iter().collect(),
            ..State::default()
        },
    };
    assert!(
        !controller
            .prepare_managed_raw_worktree(&session_id, &ProcessExecutor)
            .unwrap()
    );
    let session = &controller.state.sessions[&session_id];
    assert_eq!(session.project_directory.as_deref(), Some(plain.path()));
    assert!(session.managed_worktree.is_none());
    assert!(!plain.path().join(".git").exists());
}

#[test]
fn raw_linked_worktree_origin_matches_the_configured_github_project() {
    struct OriginExecutor;
    impl CommandExecutor for OriginExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            assert_eq!(
                command.args,
                [
                    "-C",
                    "/mnt/optane/bifrost-fird",
                    "config",
                    "--get",
                    "remote.origin.url",
                ]
            );
            Ok(CommandOutput {
                status: 0,
                stdout: b"git@github.com:BrokkAi/bifrost-dev.git\n".to_vec(),
                stderr: Vec::new(),
            })
        }
    }

    let mut config = Config::default();
    config
        .targets
        .insert("localhost".into(), TargetTemplate::LocalBare);
    let session = raw_session_on("localhost", "/mnt/optane/bifrost-fird");
    let session_id = session.id.clone();
    let controller = Controller {
        config,
        state: State {
            sessions: [(session_id.clone(), session)].into_iter().collect(),
            ..State::default()
        },
    };

    let source = controller
        .resolve_session_project_source(&session_id, &OriginExecutor)
        .unwrap();

    assert_eq!(source.key, "github:brokkai/bifrost-dev");
    assert_eq!(source.short, "bifrost-dev");
    assert_eq!(source.full, "BrokkAi/bifrost-dev");
}
#[test]
fn managed_worktree_origin_uses_source_repository_while_checkout_is_retired() {
    struct OriginExecutor;
    impl CommandExecutor for OriginExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            assert_eq!(
                command.args,
                [
                    "-C",
                    "/home/dev/project",
                    "config",
                    "--get",
                    "remote.origin.url",
                ]
            );
            Ok(CommandOutput {
                status: 0,
                stdout: b"git@github.com:example/project.git\n".to_vec(),
                stderr: Vec::new(),
            })
        }
    }

    let session = managed_raw_session(ManagedWorktreeTarget::Local);
    let session_id = session.id.clone();
    let controller = Controller {
        config: Config::default(),
        state: State {
            sessions: [(session_id.clone(), session)].into_iter().collect(),
            ..State::default()
        },
    };

    let source = controller
        .resolve_session_project_source(&session_id, &OriginExecutor)
        .unwrap();

    assert_eq!(source.key, "github:example/project");
}

#[test]
fn raw_no_origin_uses_the_canonical_main_repository_root() {
    struct NoOriginExecutor {
        commands: RefCell<Vec<CommandSpec>>,
    }
    impl CommandExecutor for NoOriginExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.commands.borrow_mut().push(command.clone());
            if command.args.iter().any(|argument| argument == "config") {
                return Ok(CommandOutput {
                    status: 1,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                });
            }
            let stdout = if command
                .args
                .iter()
                .any(|argument| argument == "--show-toplevel")
            {
                "/worktrees/project-side\n"
            } else if command
                .args
                .iter()
                .any(|argument| argument == "--git-common-dir")
            {
                "/projects/project/.git\n"
            } else {
                panic!("unexpected command {:?}", command.args);
            };
            Ok(CommandOutput {
                status: 0,
                stdout: stdout.as_bytes().to_vec(),
                stderr: Vec::new(),
            })
        }
    }

    let mut config = Config::default();
    config
        .targets
        .insert("localhost".into(), TargetTemplate::LocalBare);
    let session = raw_session_on("localhost", "/worktrees/project-side");
    let session_id = session.id.clone();
    let controller = Controller {
        config,
        state: State {
            sessions: [(session_id.clone(), session)].into_iter().collect(),
            ..State::default()
        },
    };
    let executor = NoOriginExecutor {
        commands: RefCell::new(Vec::new()),
    };

    let source = controller
        .resolve_session_project_source(&session_id, &executor)
        .unwrap();

    assert_eq!(source.key, "path:/projects/project");
    assert_eq!(source.short, "project");
    assert_eq!(source.full, "/projects/project");
    assert_eq!(executor.commands.borrow().len(), 3);
}

#[test]
fn raw_non_git_directory_keeps_its_local_path_source() {
    struct NonGitExecutor {
        commands: RefCell<Vec<CommandSpec>>,
    }
    impl CommandExecutor for NonGitExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.commands.borrow_mut().push(command.clone());
            let status = if command.args.iter().any(|argument| argument == "config") {
                1
            } else {
                assert!(
                    command
                        .args
                        .iter()
                        .any(|argument| argument == "--show-toplevel")
                );
                128
            };
            Ok(CommandOutput {
                status,
                stdout: Vec::new(),
                stderr: b"fatal: not a git repository\n".to_vec(),
            })
        }
    }

    let mut config = Config::default();
    config
        .targets
        .insert("localhost".into(), TargetTemplate::LocalBare);
    let session = raw_session_on("localhost", "/scratch/project");
    let session_id = session.id.clone();
    let controller = Controller {
        config,
        state: State {
            sessions: [(session_id.clone(), session)].into_iter().collect(),
            ..State::default()
        },
    };
    let executor = NonGitExecutor {
        commands: RefCell::new(Vec::new()),
    };

    let source = controller
        .resolve_session_project_source(&session_id, &executor)
        .unwrap();

    assert_eq!(source.key, "path:/scratch/project");
    assert_eq!(source.full, "/scratch/project");
    assert_eq!(executor.commands.borrow().len(), 2);
}

#[test]
fn project_root_lookup_reports_git_failures_instead_of_treating_them_as_non_git() {
    struct FailedGit;
    impl CommandExecutor for FailedGit {
        fn execute(&self, _: &CommandSpec) -> Result<CommandOutput> {
            Ok(CommandOutput {
                status: 128,
                stdout: Vec::new(),
                stderr: b"fatal: detected dubious ownership in repository".to_vec(),
            })
        }
    }
    let error = resolve_git_root(
        &ManagedWorktreeTarget::Local,
        Path::new("/project"),
        &FailedGit,
    )
    .unwrap_err();
    assert!(error.to_string().contains("dubious ownership"));
}

/// Answers the two Git reads that locate a checkout, and nothing else.
struct CheckoutPositionExecutor {
    head_commit: String,
    branch: Option<String>,
}
impl CommandExecutor for CheckoutPositionExecutor {
    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        let stdout = if command.args.iter().any(|argument| argument == "rev-parse") {
            self.head_commit.clone()
        } else if command
            .args
            .iter()
            .any(|argument| argument == "symbolic-ref")
        {
            match &self.branch {
                Some(branch) => branch.clone(),
                None => {
                    return Ok(CommandOutput {
                        status: 1,
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                    });
                }
            }
        } else {
            panic!("unexpected command {:?}", command.args);
        };
        Ok(CommandOutput {
            status: 0,
            stdout: format!("{stdout}\n").into_bytes(),
            stderr: Vec::new(),
        })
    }
}
fn recorded_repository(head_commit: &str, branch: Option<&str>) -> RepositoryMetadata {
    RepositoryMetadata {
        push_urls: Vec::new(),
        remote_workspace: false,
        id: "project".into(),
        relative_destination: PathBuf::from("project"),
        origin: "mj-local:project".into(),
        base_commit: String::new(),
        head_commit: head_commit.into(),
        branch: branch.map(str::to_owned),
    }
}
#[test]
fn a_raw_checkout_that_moved_while_stopped_gets_a_conversation_line() {
    let config = resume_compatibility_config();
    let session = managed_raw_session(ManagedWorktreeTarget::Local);
    let directory = session.project_directory.clone().unwrap();
    let executor = CheckoutPositionExecutor {
        head_commit: "b".repeat(40),
        branch: Some("mj/0123456789abcdef0123456789abcdef".into()),
    };

    let live = raw_checkout_position(&session, &config, &directory, &executor).unwrap();
    let notice = raw_checkout_divergence_notice(
        &directory,
        Some(&recorded_repository(&"a".repeat(40), Some("main"))),
        &live,
    )
    .expect("a moved checkout is reported");

    assert!(
        notice.contains(&directory.display().to_string()),
        "{notice}"
    );
    assert!(notice.contains("aaaaaaaaaaaa (main)"), "{notice}");
    assert!(
        notice.contains("bbbbbbbbbbbb (mj/0123456789abcdef0123456789abcdef)"),
        "{notice}"
    );
    assert!(
        notice.contains("while this session was stopped"),
        "{notice}"
    );
}
#[test]
fn a_raw_checkout_that_stayed_put_gets_no_conversation_line() {
    let config = resume_compatibility_config();
    let session = managed_raw_session(ManagedWorktreeTarget::Local);
    let directory = session.project_directory.clone().unwrap();
    let executor = CheckoutPositionExecutor {
        head_commit: "a".repeat(40),
        branch: Some("main".into()),
    };

    let live = raw_checkout_position(&session, &config, &directory, &executor).unwrap();

    assert_eq!(
        raw_checkout_divergence_notice(
            &directory,
            Some(&recorded_repository(&"a".repeat(40), Some("main"))),
            &live,
        ),
        None
    );
}
#[test]
fn a_checkpoint_without_recorded_git_identity_reports_nothing() {
    let live = CheckoutPosition {
        head_commit: "b".repeat(40),
        branch: None,
    };

    assert_eq!(
        raw_checkout_divergence_notice(Path::new("/home/dev/project"), None, &live),
        None
    );
    assert_eq!(
        raw_checkout_divergence_notice(
            Path::new("/home/dev/project"),
            Some(&recorded_repository("", None)),
            &live,
        ),
        None
    );
}
#[test]
fn a_detached_checkout_is_named_as_detached() {
    let config = resume_compatibility_config();
    let session = managed_raw_session(ManagedWorktreeTarget::Local);
    let directory = session.project_directory.clone().unwrap();
    let executor = CheckoutPositionExecutor {
        head_commit: "c".repeat(40),
        branch: None,
    };

    let live = raw_checkout_position(&session, &config, &directory, &executor).unwrap();
    let notice = raw_checkout_divergence_notice(
        &directory,
        Some(&recorded_repository(&"a".repeat(40), Some("main"))),
        &live,
    )
    .expect("a moved checkout is reported");

    assert!(notice.contains("cccccccccccc (detached)"), "{notice}");
}
#[test]
fn bundle_sessions_resume_on_any_workspace_target() {
    let config = resume_compatibility_config();
    let session = checkpoint_test_session("0123456789abcdef0123456789abcdef");

    assert_eq!(
        resume_compatibility(&session, &config, "podman"),
        Ok(ResumePlan::InPlace)
    );
    assert_eq!(
        resume_compatibility(&session, &config, "ssh-bare"),
        Ok(ResumePlan::InPlace)
    );
}
#[test]
fn a_single_local_repository_can_become_a_checkout() {
    let mut config = resume_compatibility_config();
    let mut session = checkpoint_test_session("0123456789abcdef0123456789abcdef");
    session.bundle_id = "project".into();
    config.bundles.insert(
        "project".into(),
        local_bundle(Path::new("/home/dev/project")),
    );

    assert_eq!(
        resume_compatibility(&session, &config, "local-bare"),
        Ok(ResumePlan::WorkspaceToRaw)
    );
}
#[test]
fn a_github_project_cannot_become_a_checkout() {
    let mut config = resume_compatibility_config();
    let mut session = checkpoint_test_session("0123456789abcdef0123456789abcdef");
    session.bundle_id = "project".into();
    let mut bundle = local_bundle(Path::new("/home/dev/project"));
    bundle.repositories[0].local = None;
    bundle.repositories[0].github = Some("example/project".into());
    config.bundles.insert("project".into(), bundle);

    let reason = resume_compatibility(&session, &config, "local-bare").unwrap_err();

    assert!(reason.contains("came from GitHub"), "{reason}");
    assert!(
        reason.contains("resume it on a container, SSH, or EC2 target"),
        "{reason}"
    );
}
#[test]
fn a_multi_repository_project_cannot_become_a_checkout() {
    let mut config = resume_compatibility_config();
    let mut session = checkpoint_test_session("0123456789abcdef0123456789abcdef");
    session.bundle_id = "project".into();
    let mut bundle = local_bundle(Path::new("/home/dev/project"));
    bundle.repositories.push(ProjectRepository {
        id: "tools".into(),
        github: None,
        local: Some(PathBuf::from("/home/dev/tools")),
        destination: PathBuf::from("tools"),
        git_ref: None,
    });
    config.bundles.insert("project".into(), bundle);

    let reason = resume_compatibility(&session, &config, "local-bare").unwrap_err();

    assert!(reason.contains("2 repositories"), "{reason}");
    assert!(reason.contains("one checkout"), "{reason}");
}
#[test]
fn bundle_sessions_refuse_a_local_bare_target_with_a_reason() {
    let config = resume_compatibility_config();
    let session = checkpoint_test_session("0123456789abcdef0123456789abcdef");

    let reason = resume_compatibility(&session, &config, "local-bare").unwrap_err();

    assert!(reason.contains("created from a project bundle"), "{reason}");
    assert!(
        reason.contains("resume it on a container, SSH, or EC2 target"),
        "{reason}"
    );
}
#[test]
fn managed_raw_sessions_resume_on_their_own_worktree_host() {
    let config = resume_compatibility_config();

    assert_eq!(
        resume_compatibility(
            &managed_raw_session(ManagedWorktreeTarget::Local),
            &config,
            "local-bare",
        ),
        Ok(ResumePlan::InPlace)
    );
    assert_eq!(
        resume_compatibility(
            &managed_raw_session(ssh_worktree_target()),
            &config,
            "ssh-bare",
        ),
        Ok(ResumePlan::InPlace)
    );
}
#[test]
fn managed_raw_sessions_refuse_a_bare_target_on_another_host() {
    let config = resume_compatibility_config();

    let reason = resume_compatibility(
        &managed_raw_session(ManagedWorktreeTarget::Local),
        &config,
        "ssh-bare",
    )
    .unwrap_err();
    assert!(reason.contains("this machine"), "{reason}");

    let reason = resume_compatibility(
        &managed_raw_session(ssh_worktree_target()),
        &config,
        "local-bare",
    )
    .unwrap_err();
    assert!(reason.contains("dev@builder"), "{reason}");
}
#[test]
fn a_whole_local_checkout_can_move_to_an_isolated_target() {
    let config = resume_compatibility_config();
    for session in [
        managed_raw_session(ManagedWorktreeTarget::Local),
        raw_session_on("local-bare", "/home/dev/project"),
    ] {
        assert_eq!(
            resume_compatibility(&session, &config, "podman"),
            Ok(ResumePlan::RawToWorkspace)
        );
    }
}
/// Give a checkout the network remote a conversion requires. Planning
/// never contacts it.
fn add_network_remote(checkout: &Path) {
    test_git(checkout, &["remote", "add", "origin", FIXTURE_FETCH_URL]);
}

fn fixture_network_source() -> mj_core::remote_git::NetworkGitSource {
    mj_core::remote_git::NetworkGitSource {
        fetch_url: FIXTURE_FETCH_URL.to_owned(),
        push_urls: vec![FIXTURE_FETCH_URL.to_owned()],
    }
}

#[test]
fn a_checkout_without_a_network_remote_cannot_be_planned_for_a_target() {
    let repository = committed_repository();
    let config = resume_compatibility_config();
    let session = raw_session_on("local-bare", &repository.path().to_string_lossy());

    let error = plan_raw_to_workspace(&session, &config, &ProcessExecutor).unwrap_err();

    let detail = format!("{error:#}");
    assert!(detail.contains("has no network Git remote"), "{detail}");
    assert!(detail.contains("git remote add origin"), "{detail}");
    assert!(detail.contains("bare target"), "{detail}");
}

#[test]
fn planning_a_conversion_records_the_checkouts_network_remote() {
    let (checkout, _remote_parent, _remote) = checkout_with_network_remote();
    let config = resume_compatibility_config();
    let session = raw_session_on("local-bare", &checkout.path().to_string_lossy());

    let conversion = plan_raw_to_workspace(&session, &config, &ProcessExecutor).unwrap();

    assert_eq!(conversion.source.fetch_url, FIXTURE_FETCH_URL);
    assert_eq!(conversion.source.push_urls, [FIXTURE_FETCH_URL]);
    assert_eq!(conversion.checkout, checkout.path().canonicalize().unwrap());
    assert!(conversion.retire.is_none());
}

#[test]
fn a_raw_checkout_snapshot_restores_into_a_fresh_clone_of_its_remote() {
    let (checkout, _remote_parent, remote) = checkout_with_network_remote();
    let pushed = test_git(checkout.path(), &["rev-parse", "HEAD"]);
    // A session commit on top of the pushed base has to travel in the
    // snapshot, and its prerequisite has to stay on the remote.
    std::fs::write(checkout.path().join("nested/file.txt"), "session commit\n").unwrap();
    test_git(checkout.path(), &["commit", "-am", "session commit"]);
    let head = test_git(checkout.path(), &["rev-parse", "HEAD"]);
    std::fs::write(checkout.path().join("staged.txt"), "staged\n").unwrap();
    test_git(checkout.path(), &["add", "staged.txt"]);
    std::fs::write(checkout.path().join("nested/file.txt"), "unstaged\n").unwrap();
    // Larger than a pipe buffer, so a truncated untracked capture cannot
    // pass on a toy fixture.
    let untracked = "u".repeat(100 * 1024);
    std::fs::write(checkout.path().join("untracked.txt"), &untracked).unwrap();
    let source =
        mj_core::remote_git::resolve_local_repository(checkout.path(), &ProcessExecutor).unwrap();

    let snapshot = raw_checkout_snapshot(
        checkout.path(),
        &source,
        Path::new("project"),
        &mj_checkpoint::archive::SystemGit,
    )
    .unwrap();

    assert!(snapshot.metadata.remote_workspace);
    assert_eq!(snapshot.metadata.origin, FIXTURE_FETCH_URL);
    assert_eq!(snapshot.metadata.push_urls, [FIXTURE_FETCH_URL]);
    assert_eq!(snapshot.metadata.base_commit, pushed);
    assert_eq!(snapshot.metadata.head_commit, head);
    assert_eq!(snapshot.metadata.branch.as_deref(), Some("master"));

    // A fresh clone of the remote is what the container really gets, so
    // every bundle prerequisite has to be reachable from its origin refs.
    let fresh_parent = tempfile::tempdir().unwrap();
    let fresh = fresh_parent.path().join("workspace");
    let output = Command::new("git")
        .arg("clone")
        .arg(&remote)
        .arg(&fresh)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    mj_checkpoint::archive::restore_git_snapshot(
        &mj_checkpoint::archive::SystemGit,
        &fresh,
        &snapshot,
    )
    .unwrap();

    assert_eq!(test_git(&fresh, &["rev-parse", "HEAD"]), head);
    assert_eq!(test_git(&fresh, &["branch", "--show-current"]), "master");
    assert_eq!(
        test_git(&fresh, &["diff", "--cached", "--name-only"]),
        "staged.txt"
    );
    assert_eq!(
        test_git(&fresh, &["diff", "--name-only"]),
        "nested/file.txt"
    );
    assert_eq!(
        std::fs::read_to_string(fresh.join("nested/file.txt")).unwrap(),
        "unstaged\n"
    );
    assert_eq!(
        std::fs::read_to_string(fresh.join("untracked.txt")).unwrap(),
        untracked
    );
    assert_eq!(
        test_git(&fresh, &["config", "--local", "mj.remoteWorkspace"]),
        "true"
    );
    assert_eq!(
        test_git(&fresh, &["config", "--local", "mj.baseCommit"]),
        pushed
    );
}

#[test]
fn a_conversion_preview_counts_unpushed_commits_and_dirty_files() {
    let (checkout, _remote_parent, remote) = checkout_with_network_remote();
    std::fs::write(checkout.path().join("nested/file.txt"), "session commit\n").unwrap();
    test_git(checkout.path(), &["commit", "-am", "session commit"]);
    std::fs::write(checkout.path().join("staged.txt"), "staged\n").unwrap();
    test_git(checkout.path(), &["add", "staged.txt"]);
    std::fs::write(checkout.path().join("nested/file.txt"), "unstaged\n").unwrap();
    let untracked = "u".repeat(100 * 1024);
    std::fs::write(checkout.path().join("untracked.txt"), &untracked).unwrap();
    let config = resume_compatibility_config();
    let session = raw_session_on("local-bare", &checkout.path().to_string_lossy());
    let executor = FixtureRemoteExecutor { remote };
    let conversion = plan_raw_to_workspace(&session, &config, &executor).unwrap();

    let preview = raw_conversion_preview(&session, &conversion, &executor).unwrap();

    assert_eq!(preview.fetch_url, FIXTURE_FETCH_URL);
    assert_eq!(preview.default_branch, "master");
    assert_eq!(preview.branch.as_deref(), Some("master"));
    assert_eq!(preview.unpushed_commits, 1);
    assert_eq!(preview.staged_files, 1);
    assert_eq!(preview.unstaged_files, 1);
    assert_eq!(preview.untracked_files, 1);
    assert_eq!(preview.untracked_bytes, untracked.len() as u64);
    assert!(
        preview.host_checkout_retained,
        "the user's own checkout stays on this machine"
    );
    assert_eq!(
        preview.destination,
        mj_core::targets::new_container_workspace(&session.id)
            .unwrap()
            .join(checkout.path().file_name().unwrap())
    );
}

#[test]
fn a_conversion_preview_reports_a_managed_worktree_as_not_retained() {
    let (checkout, _remote_parent, remote) = checkout_with_network_remote();
    let session_id = "0123456789abcdef0123456789abcdef";
    let session = managed_worktree_session(checkout.path(), session_id);
    let config = resume_compatibility_config();
    let executor = FixtureRemoteExecutor { remote };
    let conversion = plan_raw_to_workspace(&session, &config, &executor).unwrap();

    let preview = raw_conversion_preview(&session, &conversion, &executor).unwrap();

    assert!(
        !preview.host_checkout_retained,
        "a managed worktree is retired by the move"
    );
    assert_eq!(preview.branch, Some(format!("mj/{session_id}")));
    assert_eq!(preview.unpushed_commits, 0);
    assert_eq!(preview.staged_files, 0);
    assert_eq!(preview.unstaged_files, 0);
    assert_eq!(
        preview.destination,
        mj_core::targets::new_container_workspace(session_id)
            .unwrap()
            .join(session_id)
    );
}

#[test]
fn a_conversion_preview_refuses_a_dirty_submodule() {
    let (checkout, _remote_parent, _remote) = checkout_with_network_remote();
    let submodule = committed_repository();
    test_git(
        checkout.path(),
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            &submodule.path().to_string_lossy(),
            "sub",
        ],
    );
    test_git(checkout.path(), &["commit", "-m", "add submodule"]);
    std::fs::write(checkout.path().join("sub/nested/file.txt"), "dirty\n").unwrap();
    let config = resume_compatibility_config();
    let session = raw_session_on("local-bare", &checkout.path().to_string_lossy());
    let conversion = plan_raw_to_workspace(&session, &config, &ProcessExecutor).unwrap();

    let error = raw_conversion_preview(&session, &conversion, &ProcessExecutor).unwrap_err();

    assert!(
        format!("{error:#}").contains("dirty submodule"),
        "{error:#}"
    );
}
#[test]
fn a_raw_checkout_on_an_ssh_host_cannot_convert() {
    let config = resume_compatibility_config();

    let reason = resume_compatibility(
        &managed_raw_session(ssh_worktree_target()),
        &config,
        "podman",
    )
    .unwrap_err();
    assert!(reason.contains("works directly in"), "{reason}");
    assert!(reason.contains("dev@builder"), "{reason}");

    let reason = resume_compatibility(
        &raw_session_on("ssh-bare", "/srv/project"),
        &config,
        "podman",
    )
    .unwrap_err();
    assert!(reason.contains("on an SSH host"), "{reason}");
}
#[test]
fn a_session_that_opens_a_subdirectory_of_its_worktree_cannot_convert() {
    let config = resume_compatibility_config();
    let mut session = managed_raw_session(ManagedWorktreeTarget::Local);
    let worktree = session.managed_worktree.as_mut().unwrap();
    worktree.source_project_directory = worktree.source_repository.join("crate");
    session.project_directory = Some(worktree.worktree_root.join("crate"));

    let reason = resume_compatibility(&session, &config, "podman").unwrap_err();

    assert!(reason.contains("subdirectory of its checkout"), "{reason}");
}
#[test]
fn unmanaged_raw_sessions_require_the_same_bare_target_kind() {
    let config = resume_compatibility_config();
    let local = raw_session_on("local-bare", "/home/dev/project");
    let remote = raw_session_on("ssh-bare", "/srv/project");

    assert_eq!(
        resume_compatibility(&local, &config, "local-bare"),
        Ok(ResumePlan::InPlace)
    );
    assert_eq!(
        resume_compatibility(&remote, &config, "ssh-bare"),
        Ok(ResumePlan::InPlace)
    );
    for (session, target) in [(&local, "ssh-bare"), (&remote, "local-bare")] {
        let reason = resume_compatibility(session, &config, target).unwrap_err();
        assert!(reason.contains("directly on its host"), "{reason}");
    }
}
#[test]
fn resume_compatibility_names_a_target_that_is_gone() {
    let config = resume_compatibility_config();
    let session = checkpoint_test_session("0123456789abcdef0123456789abcdef");

    let reason = resume_compatibility(&session, &config, "retired").unwrap_err();

    assert!(reason.contains("retired"), "{reason}");
}
#[test]
fn managed_raw_worktree_inherits_upstream_and_cleans_up_owned_artifacts() {
    let repository = committed_repository();
    let remote_parent = tempfile::tempdir().unwrap();
    let remote = remote_parent.path().join("remote.git");
    let output = Command::new("git")
        .args(["init", "--bare"])
        .arg(&remote)
        .output()
        .unwrap();
    assert!(output.status.success());
    test_git(
        repository.path(),
        &["remote", "add", "origin", &remote.to_string_lossy()],
    );
    test_git(
        repository.path(),
        &["push", "--set-upstream", "origin", "master"],
    );

    let target = ManagedWorktreeTarget::Local;
    let inspection =
        inspect_raw_project(&ProcessExecutor, &target, &repository.path().join("nested")).unwrap();
    assert!(inspection.primary_checkout);
    assert_eq!(inspection.upstream.as_deref(), Some("origin/master"));
    // git rev-parse canonicalizes symlinks (macOS tempdirs live behind the
    // /var -> /private/var link), so compare against the canonical path.
    assert_eq!(
        inspection.source_project_directory,
        repository.path().canonicalize().unwrap().join("nested")
    );

    let session_id = "0123456789abcdef0123456789abcdef";
    let worktree = ManagedWorktree {
        source_project_directory: inspection.source_project_directory,
        source_repository: inspection.source_repository,
        worktree_root: repository.path().join(".mj/worktrees").join(session_id),
        branch: format!("mj/{session_id}"),
        target,
        base_commit: None,
    };
    create_managed_worktree(
        &ProcessExecutor,
        &worktree,
        inspection.upstream.as_deref(),
        PrimaryCheckoutRequirement::Clean,
    )
    .unwrap();
    assert!(worktree.worktree_root.join("nested/file.txt").is_file());
    assert_eq!(
        test_git(
            &worktree.worktree_root,
            &[
                "rev-parse",
                "--abbrev-ref",
                "--symbolic-full-name",
                "@{upstream}"
            ]
        ),
        "origin/master"
    );
    assert_eq!(test_git(repository.path(), &["status", "--porcelain"]), "");
    std::fs::write(worktree.worktree_root.join("dirty.txt"), "session\n").unwrap();

    cleanup_managed_worktree(&ProcessExecutor, &worktree, BranchDisposition::Delete).unwrap();
    assert!(!worktree.worktree_root.exists());
    assert!(!repository.path().join(".mj").exists());
    let output = Command::new("git")
        .arg("-C")
        .arg(repository.path())
        .args([
            "show-ref",
            "--verify",
            &format!("refs/heads/{}", worktree.branch),
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
}
#[test]
fn retired_worktree_can_be_recreated_from_its_retained_branch() {
    let repository = committed_repository();
    let session_id = "0123456789abcdef0123456789abcdef";
    let session = managed_worktree_session(repository.path(), session_id);
    let worktree = session.managed_worktree.unwrap();
    // The checkout is dirty by design: its dirty state moved into the target.
    std::fs::write(worktree.worktree_root.join("dirty.txt"), "session\n").unwrap();

    retire_managed_worktree(&ProcessExecutor, &worktree).unwrap();

    assert!(!worktree.worktree_root.exists());
    assert!(!repository.path().join(".mj").exists());
    let branch = Command::new("git")
        .arg("-C")
        .arg(repository.path())
        .args([
            "show-ref",
            "--verify",
            &format!("refs/heads/{}", worktree.branch),
        ])
        .output()
        .unwrap();
    assert!(
        branch.status.success(),
        "the session branch must survive: later checkpoints are deltas against it"
    );

    assert!(restore_managed_worktree(&ProcessExecutor, &worktree).unwrap());
    assert!(worktree.worktree_root.join("nested/file.txt").is_file());
    assert!(!restore_managed_worktree(&ProcessExecutor, &worktree).unwrap());
}
#[test]
fn retiring_a_remote_worktree_prunes_registration_after_target_removed_checkout() {
    struct RemoteExecutor {
        path_checks: RefCell<usize>,
        commands: RefCell<Vec<CommandSpec>>,
    }

    impl CommandExecutor for RemoteExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.commands.borrow_mut().push(command.clone());
            let status = if command.purpose == "check managed worktree path" {
                let mut checks = self.path_checks.borrow_mut();
                let status = i32::from(*checks != 0);
                *checks += 1;
                status
            } else {
                0
            };
            Ok(CommandOutput {
                status,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    let worktree = ManagedWorktree {
        source_project_directory: PathBuf::from("/srv/project"),
        source_repository: PathBuf::from("/srv/project"),
        worktree_root: PathBuf::from("/srv/project/.mj/worktrees/session"),
        branch: "mj/session".into(),
        target: ManagedWorktreeTarget::Ssh {
            destination: "builder".into(),
            ssh_args: Vec::new(),
        },
        base_commit: None,
    };
    let executor = RemoteExecutor {
        path_checks: RefCell::new(0),
        commands: RefCell::new(Vec::new()),
    };

    retire_managed_worktree(&executor, &worktree).unwrap();

    let purposes = executor
        .commands
        .borrow()
        .iter()
        .map(|command| command.purpose.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        purposes,
        [
            "check managed worktree path",
            "check managed worktree path",
            "prune managed worktree metadata",
            "remove empty managed worktree directories",
        ]
    );
}
#[test]
fn a_managed_conversion_carries_the_session_worktree_not_the_primary_checkout() {
    let repository = committed_repository();
    add_network_remote(repository.path());
    let session_id = "0123456789abcdef0123456789abcdef";
    let session = managed_worktree_session(repository.path(), session_id);
    let worktree = session.managed_worktree.clone().unwrap();

    let conversion = plan_raw_to_workspace(&session, &Config::default(), &ProcessExecutor).unwrap();

    assert_eq!(conversion.checkout, worktree.worktree_root);
    assert_eq!(conversion.repository, repository.path());
    assert_eq!(conversion.retire, Some(worktree));
    let bundle = conversion.new_bundle.expect("a bundle is synthesized");
    assert_eq!(bundle.repositories.len(), 1);
    assert_eq!(bundle.primary_repo, bundle.repositories[0].id);
    assert_eq!(
        bundle.repositories[0].local.as_deref(),
        Some(repository.path())
    );
    assert_eq!(bundle.repositories[0].github, None);
    // The archive names the session directory as the repository, and the
    // restored harness session points inside the target at that name.
    assert_eq!(
        bundle.repositories[0].destination,
        PathBuf::from(session_id)
    );
}
#[test]
fn an_unmanaged_conversion_serves_the_main_repository_behind_a_linked_worktree() {
    let repository = committed_repository();
    add_network_remote(repository.path());
    let session_id = "0123456789abcdef0123456789abcdef";
    let linked = managed_worktree_session(repository.path(), session_id);
    let checkout = linked.managed_worktree.unwrap().worktree_root;
    let mut session = checkpoint_test_session(session_id);
    session.state = SessionState::Stopped;
    session.target_template_id = "local-bare".into();
    session.project_directory = Some(checkout.clone());

    let conversion = plan_raw_to_workspace(&session, &Config::default(), &ProcessExecutor).unwrap();

    assert_eq!(conversion.checkout, checkout.canonicalize().unwrap());
    assert_eq!(
        conversion.repository,
        repository.path().canonicalize().unwrap()
    );
    assert_eq!(conversion.retire, None);
}
/// The recorded project directory may reach the checkout through a
/// symlink, as the system temp directory does on macOS. Git reports
/// canonical paths, so the whole-checkout rule must not compare across
/// the two domains.
#[cfg(unix)]
#[test]
fn an_unmanaged_conversion_accepts_a_checkout_reached_through_a_symlink() {
    let repository = committed_repository();
    add_network_remote(repository.path());
    let session_id = "0123456789abcdef0123456789abcdef";
    let linked = managed_worktree_session(repository.path(), session_id);
    let checkout = linked.managed_worktree.unwrap().worktree_root;
    let alias = tempfile::tempdir().unwrap();
    let symlink = alias.path().join("checkout");
    std::os::unix::fs::symlink(&checkout, &symlink).unwrap();
    let mut session = checkpoint_test_session(session_id);
    session.state = SessionState::Stopped;
    session.target_template_id = "local-bare".into();
    session.project_directory = Some(symlink);

    let conversion = plan_raw_to_workspace(&session, &Config::default(), &ProcessExecutor).unwrap();

    assert_eq!(conversion.checkout, checkout.canonicalize().unwrap());
    assert_eq!(
        conversion.repository,
        repository.path().canonicalize().unwrap()
    );
    assert_eq!(conversion.retire, None);
}
#[test]
fn a_conversion_reuses_a_bundle_that_already_describes_the_checkout() {
    let repository = PathBuf::from("/home/dev/project");
    let destination = PathBuf::from("project");
    let existing = ProjectBundle {
        primary_repo: "project".into(),
        repositories: vec![ProjectRepository {
            id: "project".into(),
            github: None,
            local: Some(repository.clone()),
            destination: destination.clone(),
            git_ref: None,
        }],
    };
    let mut config = Config::default();
    config.bundles.insert("existing".into(), existing);

    assert_eq!(
        converted_raw_bundle(&config, "remote-project-abcdef", &repository, &destination),
        ("existing".to_owned(), None)
    );

    // A different destination is a different checkout location inside the
    // target, so it cannot stand in for this one.
    let (id, synthesized) = converted_raw_bundle(
        &config,
        "remote-project-abcdef",
        &repository,
        Path::new("elsewhere"),
    );
    assert_ne!(id, "existing");
    assert_eq!(
        synthesized.unwrap().repositories[0].destination,
        PathBuf::from("elsewhere")
    );
}
#[test]
fn a_converted_record_is_a_valid_bundle_session() {
    let session_id = "0123456789abcdef0123456789abcdef";
    let mut config = resume_compatibility_config();
    let mut record = managed_raw_session(ManagedWorktreeTarget::Local);
    record.state = SessionState::Running;
    record.target_template_id = "podman".into();
    let conversion = RawToWorkspaceConversion {
        checkout: record.project_directory.clone().unwrap(),
        repository: PathBuf::from("/home/dev/project"),
        source: fixture_network_source(),
        bundle_id: "project".into(),
        new_bundle: Some(ProjectBundle {
            primary_repo: "project".into(),
            repositories: vec![ProjectRepository {
                id: "project".into(),
                github: None,
                local: Some(PathBuf::from("/home/dev/project")),
                destination: PathBuf::from(session_id),
                git_ref: None,
            }],
        }),
        retire: record.managed_worktree.clone(),
    };

    config.bundles.insert(
        conversion.bundle_id.clone(),
        conversion.new_bundle.clone().unwrap(),
    );
    config.profiles.insert(
        record.last_profile.clone(),
        HarnessProfile {
            enabled: true,
            kind: record.harness_kind,
            home: PathBuf::from("/profiles/codex"),
            environment: BTreeMap::new(),
            context_window_bytes: None,
            guardian_review_model: None,
        },
    );
    apply_raw_to_workspace(&mut record, &conversion);

    assert_eq!(record.project_directory, None);
    assert_eq!(record.managed_worktree, None);
    assert_eq!(record.bundle_id, "project");
    let state = State {
        sessions: BTreeMap::from([(session_id.into(), record)]),
        ..State::default()
    };
    state.validate_against_config(&config).unwrap();
}
#[test]
fn a_session_leaving_its_target_claims_a_worktree_of_its_own_repository() {
    let repository = committed_repository();
    let session_id = "0123456789abcdef0123456789abcdef";
    let mut session = checkpoint_test_session(session_id);
    session.state = SessionState::Stopped;
    session.bundle_id = "project".into();
    let mut config = resume_compatibility_config();
    config
        .bundles
        .insert("project".into(), local_bundle(repository.path()));
    let controller = Controller {
        config,
        state: State {
            sessions: BTreeMap::from([(session_id.into(), session.clone())]),
            ..State::default()
        },
    };

    let conversion = controller
        .plan_workspace_to_raw(&session, "local-bare", &ProcessExecutor)
        .unwrap();

    assert_eq!(
        conversion.worktree,
        ManagedWorktree {
            source_project_directory: repository.path().to_path_buf(),
            source_repository: repository.path().to_path_buf(),
            worktree_root: repository.path().join(".mj/worktrees").join(session_id),
            branch: format!("mj/{session_id}"),
            target: ManagedWorktreeTarget::Local,
            // The new branch starts at the repository's HEAD, which is
            // what an export of this session diffs against.
            base_commit: Some(test_git(repository.path(), &["rev-parse", "HEAD"])),
        }
    );

    // The dirty primary checkout is beside the point: the worktree's
    // contents come from the checkpoint.
    std::fs::write(repository.path().join("dirty.txt"), "primary\n").unwrap();
    create_managed_worktree(
        &ProcessExecutor,
        &conversion.worktree,
        None,
        PrimaryCheckoutRequirement::Any,
    )
    .unwrap();
    assert!(conversion.worktree.worktree_root.is_dir());

    // A second attempt refuses rather than taking over a live worktree.
    let error = controller
        .plan_workspace_to_raw(&session, "local-bare", &ProcessExecutor)
        .unwrap_err();
    assert!(format!("{error:#}").contains("already exists"), "{error:#}");
}
#[test]
fn a_return_to_local_reuses_its_retained_branch_and_preserves_its_tip() {
    let repository = committed_repository();
    let session_id = "0123456789abcdef0123456789abcdef";
    let branch = format!("mj/{session_id}");
    let original_tip = test_git(repository.path(), &["rev-parse", "HEAD"]);
    test_git(repository.path(), &["branch", &branch]);
    let mut config = resume_compatibility_config();
    config
        .bundles
        .insert("project".into(), local_bundle(repository.path()));
    let session = checkpoint_test_session(session_id);
    let controller = Controller {
        config,
        state: State {
            sessions: BTreeMap::from([(session_id.into(), session.clone())]),
            ..State::default()
        },
    };

    let conversion = controller
        .plan_workspace_to_raw(&session, "local-bare", &ProcessExecutor)
        .unwrap();

    assert!(conversion.reuse_existing_branch);
    assert!(!conversion.worktree.worktree_root.exists());
    let recovery_ref =
        preserve_retained_managed_worktree_branch(&ProcessExecutor, &conversion.worktree).unwrap();
    assert_eq!(
        recovery_ref,
        format!("refs/mj/recovery/{session_id}/{original_tip}")
    );
    assert_eq!(
        test_git(
            repository.path(),
            &["show-ref", "--hash", recovery_ref.as_str()],
        ),
        original_tip
    );
    assert_eq!(
        preserve_retained_managed_worktree_branch(&ProcessExecutor, &conversion.worktree).unwrap(),
        recovery_ref
    );

    test_git(repository.path(), &["checkout", &branch]);
    test_git(
        repository.path(),
        &["commit", "--allow-empty", "-m", "later retained tip"],
    );
    let later_tip = test_git(repository.path(), &["rev-parse", "HEAD"]);
    test_git(repository.path(), &["checkout", "master"]);
    let later_recovery_ref =
        preserve_retained_managed_worktree_branch(&ProcessExecutor, &conversion.worktree).unwrap();
    assert_eq!(
        later_recovery_ref,
        format!("refs/mj/recovery/{session_id}/{later_tip}")
    );
    assert_ne!(later_recovery_ref, recovery_ref);
    assert_eq!(
        test_git(
            repository.path(),
            &["show-ref", "--hash", recovery_ref.as_str()],
        ),
        original_tip
    );
}
#[test]
fn a_return_to_local_rejects_a_retained_branch_checked_out_elsewhere() {
    let repository = committed_repository();
    let session_id = "0123456789abcdef0123456789abcdef";
    let branch = format!("mj/{session_id}");
    test_git(repository.path(), &["branch", &branch]);
    let elsewhere = repository.path().join("other-worktree");
    test_git(
        repository.path(),
        &["worktree", "add", &elsewhere.to_string_lossy(), &branch],
    );
    let mut config = resume_compatibility_config();
    config
        .bundles
        .insert("project".into(), local_bundle(repository.path()));
    let session = checkpoint_test_session(session_id);
    let controller = Controller {
        config,
        state: State {
            sessions: BTreeMap::from([(session_id.into(), session.clone())]),
            ..State::default()
        },
    };

    let error = controller
        .plan_workspace_to_raw(&session, "local-bare", &ProcessExecutor)
        .unwrap_err();

    assert!(error.to_string().contains("still checked out"), "{error:#}");
    assert!(
        !repository
            .path()
            .join(".mj/worktrees")
            .join(session_id)
            .exists()
    );
    assert_eq!(
        test_git(repository.path(), &["rev-parse", &branch]),
        test_git(repository.path(), &["rev-parse", "HEAD"])
    );
}
#[test]
fn a_session_that_left_its_target_is_a_valid_raw_session() {
    let session_id = "0123456789abcdef0123456789abcdef";
    let repository = PathBuf::from("/home/dev/project");
    let mut config = resume_compatibility_config();
    config
        .bundles
        .insert("project".into(), local_bundle(&repository));
    config.profiles.insert(
        "codex".into(),
        HarnessProfile {
            enabled: true,
            kind: mj_core::config::HarnessKind::Codex,
            home: PathBuf::from("/profiles/codex"),
            environment: BTreeMap::new(),
            context_window_bytes: None,
            guardian_review_model: None,
        },
    );
    let mut record = checkpoint_test_session(session_id);
    record.bundle_id = "project".into();
    record.target_template_id = "local-bare".into();
    let conversion = WorkspaceToRawConversion {
        worktree: ManagedWorktree {
            source_project_directory: repository.clone(),
            source_repository: repository.clone(),
            worktree_root: repository.join(".mj/worktrees").join(session_id),
            branch: format!("mj/{session_id}"),
            target: ManagedWorktreeTarget::Local,
            base_commit: None,
        },
        reuse_existing_branch: false,
    };

    apply_workspace_to_raw(&mut record, &conversion);

    assert_eq!(
        record.project_directory.as_deref(),
        Some(conversion.worktree.worktree_root.as_path())
    );
    assert_eq!(record.bundle_id, "project", "the bundle still describes it");
    let state = State {
        sessions: BTreeMap::from([(session_id.into(), record)]),
        ..State::default()
    };
    state.validate_against_config(&config).unwrap();
}
#[test]
fn a_failed_departure_returns_the_session_to_its_bundle() {
    let session_id = "0123456789abcdef0123456789abcdef";
    let repository = PathBuf::from("/home/dev/project");
    let previous = {
        let mut record = checkpoint_test_session(session_id);
        record.state = SessionState::Stopped;
        record.bundle_id = "project".into();
        record
    };
    let mut converted = previous.clone();
    converted.state = SessionState::Provisioning;
    apply_workspace_to_raw(
        &mut converted,
        &WorkspaceToRawConversion {
            worktree: ManagedWorktree {
                source_project_directory: repository.clone(),
                source_repository: repository.clone(),
                worktree_root: repository.join(".mj/worktrees").join(session_id),
                branch: format!("mj/{session_id}"),
                target: ManagedWorktreeTarget::Local,
                base_commit: None,
            },
            reuse_existing_branch: false,
        },
    );

    apply_failed_resume_rollback(&mut converted, &previous, "podman is unavailable", None);

    assert_eq!(converted.project_directory, None);
    assert_eq!(converted.managed_worktree, None);
    assert_eq!(converted.bundle_id, "project");
}
#[test]
fn a_failed_conversion_returns_the_session_to_its_checkout() {
    let previous = managed_raw_session(ManagedWorktreeTarget::Local);
    let mut converted = previous.clone();
    converted.state = SessionState::Provisioning;
    converted.target_template_id = "podman".into();
    apply_raw_to_workspace(
        &mut converted,
        &RawToWorkspaceConversion {
            checkout: previous.project_directory.clone().unwrap(),
            repository: PathBuf::from("/home/dev/project"),
            source: fixture_network_source(),
            bundle_id: "project".into(),
            new_bundle: None,
            retire: previous.managed_worktree.clone(),
        },
    );

    let mut cleaned = converted.clone();
    apply_failed_resume_rollback(&mut cleaned, &previous, "podman is unavailable", None);
    assert_eq!(cleaned.project_directory, previous.project_directory);
    assert_eq!(cleaned.managed_worktree, previous.managed_worktree);
    assert_eq!(cleaned.bundle_id, previous.bundle_id);

    // Even when the leftover target could not be removed, the record must
    // describe the checkout it still owns.
    let mut stranded = converted;
    apply_failed_resume_rollback(
        &mut stranded,
        &previous,
        "podman is unavailable",
        Some("podman rm failed".into()),
    );
    assert_eq!(stranded.state, SessionState::Error);
    assert_eq!(stranded.project_directory, previous.project_directory);
    assert_eq!(stranded.managed_worktree, previous.managed_worktree);
    assert_eq!(stranded.bundle_id, previous.bundle_id);
}
#[test]
fn cancelled_new_session_cleanup_removes_managed_worktree_and_branch() {
    let repository = committed_repository();
    let session_id = "0123456789abcdef0123456789abcdef";
    let worktree = ManagedWorktree {
        source_project_directory: repository.path().to_path_buf(),
        source_repository: repository.path().to_path_buf(),
        worktree_root: repository.path().join(".mj/worktrees").join(session_id),
        branch: format!("mj/{session_id}"),
        target: ManagedWorktreeTarget::Local,
        base_commit: None,
    };
    create_managed_worktree(
        &ProcessExecutor,
        &worktree,
        None,
        PrimaryCheckoutRequirement::Clean,
    )
    .unwrap();

    let mut session = checkpoint_test_session(session_id);
    session.project_directory = Some(worktree.worktree_root.clone());
    session.managed_worktree = Some(worktree.clone());
    let controller = Controller {
        config: Config::default(),
        state: State {
            sessions: BTreeMap::from([(session_id.into(), session)]),
            ..State::default()
        },
    };
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let executor = CancellableProcessExecutor::new(cancelled);

    controller
        .cleanup_new_session_worktree_after_failure(session_id, &executor)
        .unwrap();

    assert!(!worktree.worktree_root.exists());
    assert!(!repository.path().join(".mj").exists());
    let branch = Command::new("git")
        .arg("-C")
        .arg(repository.path())
        .args([
            "show-ref",
            "--verify",
            &format!("refs/heads/{}", worktree.branch),
        ])
        .output()
        .unwrap();
    assert!(!branch.status.success());
}
#[test]
fn managed_raw_worktree_refuses_dirty_primary_and_skips_existing_worktree() {
    let repository = committed_repository();
    std::fs::write(repository.path().join("dirty.txt"), "dirty\n").unwrap();
    let target = ManagedWorktreeTarget::Local;
    let inspection = inspect_raw_project(&ProcessExecutor, &target, repository.path()).unwrap();
    let session_id = "fedcba9876543210fedcba9876543210";
    let managed = ManagedWorktree {
        source_project_directory: inspection.source_project_directory,
        source_repository: inspection.source_repository,
        worktree_root: repository.path().join(".mj/worktrees").join(session_id),
        branch: format!("mj/{session_id}"),
        target: target.clone(),
        base_commit: None,
    };
    let error = create_managed_worktree(
        &ProcessExecutor,
        &managed,
        None,
        PrimaryCheckoutRequirement::Clean,
    )
    .unwrap_err();
    assert!(error.to_string().contains("uncommitted changes"));
    assert!(!managed.worktree_root.exists());

    std::fs::remove_file(repository.path().join("dirty.txt")).unwrap();
    let existing = repository.path().join("existing-worktree");
    test_git(
        repository.path(),
        &[
            "worktree",
            "add",
            "--detach",
            &existing.to_string_lossy(),
            "HEAD",
        ],
    );
    let linked = inspect_raw_project(&ProcessExecutor, &target, &existing).unwrap();
    assert!(!linked.primary_checkout);
}
#[test]
fn managed_worktree_preflight_preserves_colliding_branch_and_directory() {
    let repository = committed_repository();
    let target = ManagedWorktreeTarget::Local;
    let session_id = "abcdef0123456789abcdef0123456789";
    let branch = format!("mj/{session_id}");
    test_git(repository.path(), &["branch", &branch]);
    let worktree = ManagedWorktree {
        source_project_directory: repository.path().to_path_buf(),
        source_repository: repository.path().to_path_buf(),
        worktree_root: repository.path().join(".mj/worktrees").join(session_id),
        branch: branch.clone(),
        target,
        base_commit: None,
    };

    let error = ensure_managed_worktree_available(&ProcessExecutor, &worktree).unwrap_err();
    assert!(error.to_string().contains("branch already exists"));
    assert!(
        !test_git(
            repository.path(),
            &["show-ref", "--verify", &format!("refs/heads/{branch}")]
        )
        .is_empty()
    );
    std::fs::create_dir_all(&worktree.worktree_root).unwrap();
    let error = ensure_managed_worktree_available(&ProcessExecutor, &worktree).unwrap_err();
    assert!(error.to_string().contains("path already exists"));
    assert!(worktree.worktree_root.is_dir());
}
#[test]
fn managed_worktree_ssh_commands_preserve_hostile_path_boundaries() {
    let target = ManagedWorktreeTarget::Ssh {
        destination: "builder".into(),
        ssh_args: vec!["-o".into(), "BatchMode=yes".into()],
    };
    let command = managed_git_command(
        &target,
        Path::new("/srv/project with ' quote"),
        ["worktree", "prune"],
        "prune test",
    );
    assert_eq!(command.program, "ssh");
    assert_eq!(&command.args[..2], ["-o", "BatchMode=yes"]);
    // Connection-sharing options may sit between the target's own args and
    // the destination; the destination and quoted remote command stay last.
    assert_eq!(command.args[command.args.len() - 2], "builder");
    assert_eq!(
        command.args[command.args.len() - 1],
        "'git' '-C' '/srv/project with '\\'' quote' 'worktree' 'prune'"
    );
}
