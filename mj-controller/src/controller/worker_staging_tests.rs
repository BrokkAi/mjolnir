//! Tests of worker staging together with the session runtime and the
//! controller above it: recovery through a real plan, and credential pushes
//! into a staged home.

use mj_core::config::HarnessKind;
#[cfg(unix)]
use mj_core::hex::lower_hex;
use mj_core::targets::SshTrafficClass;
use mj_core::worker_build::verify_worker_build;
use mj_core::worker_launch::WorkerLaunchConfig;
#[cfg(unix)]
use mj_session_types::WorkerLaunchRefreshPlan;
use mj_session_types::WorkerRecoveryPlan;
#[cfg(unix)]
use sha2::{Digest, Sha256};

use super::worker_binary::launch::*;
use super::worker_binary::test_fixtures::*;
use super::worker_binary::*;
use super::*;
use crate::targets::{CommandPlan, ProcessExecutor};
#[cfg(unix)]
use crate::test_isolation::{IsolatedTest, test_name};

#[test]
#[ignore = "requires Docker, agent-dev image, MJ_WORKER_BINARY and MJ_INSTANCE=issue-1138-docker"]
fn stopped_docker_session_recovers_with_the_current_worker_build() {
    assert_eq!(mj_core::config::instance_identity(), "issue-1138-docker");
    let source =
        PathBuf::from(std::env::var_os("MJ_WORKER_BINARY").expect("portable Linux worker"));
    verify_worker_build(&source).unwrap();
    let session = mj_core::state::new_session_id().unwrap();
    let container_id = targets::resource_name(&session).unwrap();
    let locator = targets::TargetLocator::LocalDocker {
        borrowed_from: None,
        container_id: container_id.clone(),
    };
    execute_checked(
        &ProcessExecutor,
        CommandSpec::new(
            "docker",
            [
                "run",
                "--pull=never",
                "-d",
                "--name",
                &container_id,
                "--label",
                "dev.mj.managed=true",
                "--label",
                &format!("dev.mj.session={session}"),
                "--label",
                "dev.mj.instance=issue-1138-docker",
                "ghcr.io/brokkai/mjolnir/agent-dev:latest",
                "sleep",
                "infinity",
            ],
        ),
    )
    .unwrap();
    let result = (|| -> Result<()> {
        let root = targets::worker_root(&locator, &session)?;
        execute_checked(
            &ProcessExecutor,
            targets::locator_command(
                SshTrafficClass::Control,
                &locator,
                vec![
                    "sh".into(),
                    "-c".into(),
                    format!(
                        "mkdir -p {root} && printf legacy > {root}/hel && printf old-config > {root}/launch.json"
                    ),
                ],
            ),
        )?;
        let relay = tempfile::tempdir()?;
        drop(mj_worker::relay::DurableRelay::open(
            relay.path(),
            &session,
            "2.0.0",
        )?);
        execute_checked(
            &ProcessExecutor,
            CommandSpec::new(
                "docker",
                [
                    "cp".to_owned(),
                    relay.path().join(".").to_string_lossy().into_owned(),
                    format!("{container_id}:{root}"),
                ],
            ),
        )?;
        execute_checked(
            &ProcessExecutor,
            CommandSpec::new(
                "docker",
                container_upload_ownership_args("docker", &container_id, &root, &[&root]),
            ),
        )?;
        execute_checked(
            &ProcessExecutor,
            CommandSpec::new("docker", ["stop", &container_id]),
        )?;
        let launch: WorkerLaunchConfig = serde_json::from_value(serde_json::json!({
            "session_id": session, "harness": "codex", "bridge_command": "/not-used",
            "bridge_args": [], "environment": {}, "target_environment": {},
            "cwd": "/tmp", "execution_policy": "configured_approvals", "run_mode": "checkpoint_only"
        }))?;
        let plan = WorkerRecoveryPlan {
            source_target: mj_core::state::TargetLocator::LocalDocker {
                container_id: container_id.clone(),
                borrowed_from: None,
            },
            target: targets::target_recovery_plan(&locator, &session)?,
            workspace: None,
            exit_record: None,
            liveness_probe: targets::WorkerControlPlan::new(
                "probe liveness",
                vec![worker_liveness_command(&locator, &root)],
                &locator,
                &session,
            )
            .unwrap(),
            binary_refresh: worker_binary_refresh_plan(&locator, &session)?,
            launch_refresh: Some(worker_launch_refresh_plan(&locator, &session, &launch)?),
            restart: CommandPlan {
                description: "restart isolated Docker worker".into(),
                commands: vec![start_worker_command(&locator, &root)],
            }
            .into(),
        };
        crate::session_manager::recover_worker_controlled(plan, false, None, &ProcessExecutor)?;
        let installed = execute_checked(
            &ProcessExecutor,
            installed_file_digest_command(
                &locator,
                &format!("{root}/hel"),
                "check recovered worker",
            ),
        )?;
        ensure!(
            String::from_utf8(installed.stdout)?
                .starts_with(&mj_core::worker_launch::worker_executable_digest(&source)?),
            "recovery did not install the current worker"
        );
        let reconnect = targets::reconnect_plan(&locator, &session)?
            .commands
            .remove(0);
        let runtime = tokio::runtime::Runtime::new()?;
        runtime.block_on(async {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
            let mut connection = loop {
                match crate::worker_client::RelayClient::connect(&reconnect, &session).await {
                    Ok(connection) => break connection,
                    Err(error) if tokio::time::Instant::now() >= deadline => return Err(error),
                    Err(_) => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
                }
            };
            ensure!(
                connection.status().await?.checkpoint_only,
                "recovered worker must read the new launch configuration"
            );
            ensure!(
                connection.worker_build() == Some(mj_core::worker_build::worker_build_id()),
                "the recovered process must report the installed worker inputs"
            );
            Ok::<_, anyhow::Error>(())
        })?;
        Ok(())
    })();
    // Stop the owning container before removing its files, even on failure.
    let cleanup = execute_checked(
        &ProcessExecutor,
        CommandSpec::new("docker", ["rm", "-f", &container_id]),
    );
    result.unwrap();
    cleanup.unwrap();
}

#[test]
#[ignore = "requires Docker, the agent-dev image and a portable MJ_WORKER_BINARY of this build"]
fn a_container_runs_its_images_worker_through_the_hel_link() {
    let source =
        PathBuf::from(std::env::var_os("MJ_WORKER_BINARY").expect("portable Linux worker"));
    verify_worker_build(&source).unwrap();
    let fixture = podman_install_fixture();
    let session = mj_core::state::new_session_id().unwrap();
    let container_id = targets::resource_name(&session).unwrap();
    let locator = targets::TargetLocator::LocalDocker {
        borrowed_from: None,
        container_id: container_id.clone(),
    };
    execute_checked(
        &ProcessExecutor,
        CommandSpec::new(
            "docker",
            [
                "run",
                "--pull=never",
                "-d",
                "--name",
                &container_id,
                "ghcr.io/brokkai/mjolnir/agent-dev:latest",
                "sleep",
                "infinity",
            ],
        ),
    )
    .unwrap();
    let as_root = |script: &str| {
        execute_checked(
            &ProcessExecutor,
            CommandSpec::new(
                "docker",
                ["exec", "--user", "0", &container_id, "sh", "-c", script],
            ),
        )
    };
    let result = (|| -> Result<()> {
        // `:latest` carries a worker once master publishes one; start from an
        // image without it.
        as_root("rm -f /opt/mjolnir/mj-worker")?;
        ensure!(
            image_worker_build(&locator, &ProcessExecutor)?.is_none(),
            "an image without a worker must say so"
        );
        ensure!(
            WorkerInstall::for_target(&locator, &ProcessExecutor)?
                .host_binary()
                .is_some(),
            "an image without a worker gets the host worker"
        );
        // A worker that is present but cannot run is a failure, never "absent".
        as_root(
            "mkdir -p /opt/mjolnir && printf 'not a worker' > /opt/mjolnir/mj-worker && chmod 644 /opt/mjolnir/mj-worker",
        )?;
        ensure!(
            WorkerInstall::for_target(&locator, &ProcessExecutor).is_err(),
            "an unusable image worker must not be mistaken for a missing one"
        );
        execute_checked(
            &ProcessExecutor,
            CommandSpec::new(
                "docker",
                [
                    "cp".to_owned(),
                    source.to_string_lossy().into_owned(),
                    format!("{container_id}:{IMAGE_WORKER}"),
                ],
            ),
        )?;
        as_root("chown 0:0 /opt/mjolnir/mj-worker && chmod 755 /opt/mjolnir/mj-worker")?;
        ensure!(
            image_worker_build(&locator, &ProcessExecutor)?.as_deref()
                == Some(mj_core::worker_build::worker_build_id()),
            "the image's worker reports this build"
        );

        // Checkpoint-only mode starts the worker without a harness.
        let root = targets::worker_root(&locator, &session)?;
        let relay = tempfile::tempdir()?;
        drop(mj_worker::relay::DurableRelay::open(
            relay.path(),
            &session,
            "2.0.0",
        )?);
        as_root(&format!("mkdir -p {root}"))?;
        execute_checked(
            &ProcessExecutor,
            CommandSpec::new(
                "docker",
                [
                    "cp".to_owned(),
                    relay.path().join(".").to_string_lossy().into_owned(),
                    format!("{container_id}:{root}"),
                ],
            ),
        )?;
        as_root(&format!("chown -R hel:hel {root}"))?;
        let launch: WorkerLaunchConfig = serde_json::from_value(serde_json::json!({
            "session_id": session, "harness": "codex", "bridge_command": "/not-used",
            "bridge_args": [], "environment": {}, "target_environment": {},
            "cwd": "/tmp", "execution_policy": "configured_approvals", "run_mode": "checkpoint_only"
        }))?;
        launch.write(&fixture.launch_config)?;
        let cache = mj_core::target_path::text(&store_root(&locator, &session)?);
        let unused = format!("{cache}/{}/hel", "c".repeat(64));
        as_root(&format!(
            "mkdir -p {cache}/{} && cp /opt/mjolnir/mj-worker {unused} && chown -R hel:hel {cache} && touch -t 200001010000 {unused}",
            "c".repeat(64)
        ))?;
        install_worker_files(
            &ProcessExecutor,
            &locator,
            &session,
            &root,
            &format!("{root}/profile"),
            &WorkerInstall::Image,
            &fixture.launch_config,
            &fixture.ownership,
            &fixture.profile_stage,
            None,
            &[],
        )?;
        as_root(&format!("test ! -e {unused}"))?;
        execute_checked(
            &ProcessExecutor,
            CommandSpec::new(
                "docker",
                [
                    "exec",
                    &container_id,
                    "sh",
                    "-c",
                    "test \"$(id -u)\" != 0 && test \"$(readlink \"$1/hel\")\" = /opt/mjolnir/mj-worker && test -x \"$1/hel\"",
                    "sh",
                    &root,
                ],
            ),
        )
        .context("the session's hel must be a link to the image's worker")?;
        execute_checked(&ProcessExecutor, start_worker_command(&locator, &root))?;
        let reconnect = targets::reconnect_plan(&locator, &session)?
            .commands
            .remove(0);
        let runtime = tokio::runtime::Runtime::new()?;
        runtime.block_on(async {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
            let connection = loop {
                match crate::worker_client::RelayClient::connect(&reconnect, &session).await {
                    Ok(connection) => break connection,
                    Err(error) if tokio::time::Instant::now() >= deadline => return Err(error),
                    Err(_) => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
                }
            };
            ensure!(
                connection.worker_build() == Some(mj_core::worker_build::worker_build_id()),
                "the image's worker must report this controller's build"
            );
            Ok::<_, anyhow::Error>(())
        })?;
        // The daemon identifies its worker by `hel worker run --root`; the
        // link keeps that name on the process started through it.
        let liveness = execute_checked(
            &ProcessExecutor,
            worker_liveness_command(&locator, &root).unstaged_command(),
        )?;
        ensure!(
            String::from_utf8_lossy(&liveness.stdout).trim() == "alive",
            "the worker started through the link must be recognized as alive, got {:?}",
            String::from_utf8_lossy(&liveness.stdout)
        );
        Ok(())
    })();
    let cleanup = execute_checked(
        &ProcessExecutor,
        CommandSpec::new("docker", ["rm", "-f", &container_id]),
    );
    result.unwrap();
    cleanup.unwrap();
}

#[cfg(target_os = "linux")]
// Hard-won: 6fb245e38: legacy mbx relinking appended config.toml to a configuration filename.
#[test]
fn legacy_worker_upgrade_relinks_cache_configuration_without_native_mbx() {
    use std::cell::RefCell;

    use mj_core::config::ExecutionPolicy;
    use mj_core::worker_launch::HarnessRuntimePolicy;

    use crate::targets::CommandOutput;

    struct CacheLinkExecutor {
        commands: RefCell<Vec<CommandSpec>>,
        worker_bin: PathBuf,
        home: PathBuf,
    }

    impl CommandExecutor for CacheLinkExecutor {
        fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
            mj_core::targets::DeadlinePause::none()
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            self.cancellation_requested()
        }

        fn execute_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute(command)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute_with_stdin(command, input)
        }

        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.commands.borrow_mut().push(command.clone());
            let mut args = command.args[3..].to_vec();
            // Translate only the container's worker directory; run the shell
            // with the configuration path supplied by the production caller.
            args[3] = self.worker_bin.to_string_lossy().into_owned();
            let mut local = CommandSpec::new("/bin/sh", args);
            local
                .env
                .insert("HOME".into(), self.home.to_string_lossy().into_owned());
            targets::CancellableProcessExecutor::with_timeout(std::time::Duration::from_secs(5))
                .execute(&local)
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let shared_cache = directory.path().join("shared cache");
    let configuration = shared_cache.join(".mjolnir/config/mbx");
    std::fs::create_dir_all(&configuration).unwrap();
    std::fs::write(
        configuration.join("config.toml"),
        "[gc]\nmax_total_size = '50GB'\n",
    )
    .unwrap();
    let worker_bin = directory.path().join("worker/bin");
    std::fs::create_dir_all(&worker_bin).unwrap();
    let legacy_binary = worker_bin.join("mbx");
    std::fs::write(&legacy_binary, b"legacy copied mbx executable").unwrap();
    let home = directory.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let controller = crate::controller::Controller {
        config: mj_core::config::Config::default(),
        state: mj_core::state::State::default(),
    };
    let mut session = crate::controller::test_support::checkpoint_test_session("legacy-cache");
    session.build_cache = Some(mj_core::state::SessionBuildCache {
        host: "local".into(),
        directory: shared_cache,
        max_size: None,
        target_root: None,
    });
    let backend = targets::TargetLocator::LocalPodman {
        container_id: targets::resource_name(&session.id).unwrap(),
        workspace_storage: Default::default(),
        borrowed_from: None,
    };
    let launch = WorkerLaunchConfig {
        cpu_accounting: Default::default(),
        no_github_credentials: false,
        oracle: false,
        supervisor: false,
        supervisors_enabled: false,
        initial_commands: Vec::new(),
        reviewer_launch: None,
        goal_resume_request: None,
        run_mode: Default::default(),
        session_id: session.id.clone(),
        subagents: mj_core::subagent::SubagentPolicy::Native,
        handback_tool: false,
        subagent_kind: None,
        agent_mailboxes_enabled: true,
        subtle_subagents: true,
        initial_model: None,
        review_capture: false,
        review_trigger: Default::default(),
        target_environment: Default::default(),
        seed_image_environment: true,
        harness: HarnessKind::Codex,
        harness_home: directory.path().join("profile"),
        authentication_marker: None,
        bridge_command: "codex".into(),
        bridge_args: Vec::new(),
        harness_runtime: HarnessRuntimePolicy::Ambient,
        environment: Default::default(),
        excluded_environment: Vec::new(),
        cwd: directory.path().to_path_buf(),
        additional_directories: Vec::new(),
        native_session_id: None,
        project_memory: None,
        execution_policy: ExecutionPolicy::ConfiguredApprovals,
        claude_permission_mode: Default::default(),
        mjolnir_guardian: true,
        observe_policy: None,
    };
    let executor = CacheLinkExecutor {
        commands: RefCell::new(Vec::new()),
        worker_bin,
        home: home.clone(),
    };

    // This is the cache-configuration preflight called by
    // upgrade_session_worker before it prepares or swaps the worker.
    controller
        .prepare_build_cache_links(&session, &backend, &launch, &executor)
        .unwrap();

    let commands = executor.commands.borrow();
    assert_eq!(
        commands.len(),
        1,
        "legacy preparation must not probe the host"
    );
    assert_eq!(
        commands[0].purpose,
        "inspect legacy mbx and relink its shared configuration"
    );
    assert_eq!(commands[0].program, "podman");
    assert!(commands[0].args.iter().any(|argument| {
        argument.ends_with("/bin") && argument.starts_with("/var/lib/hel/workers/")
    }));
    assert_eq!(
        std::fs::read(&legacy_binary).unwrap(),
        b"legacy copied mbx executable"
    );
    assert_eq!(
        std::fs::read_link(home.join(".config/mbx/config.toml")).unwrap(),
        configuration.join("config.toml")
    );
    assert_eq!(
        std::fs::read_to_string(home.join(".config/mbx/config.toml")).unwrap(),
        "[gc]\nmax_total_size = '50GB'\n"
    );
}

#[cfg(unix)]
#[test]
fn recovery_preserves_launch_config_until_a_matching_worker_source_is_available() {
    const CHILD: &str = "MJ_RECOVERY_BINARY_PAIR_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(test_name(
            module_path!(),
            "recovery_preserves_launch_config_until_a_matching_worker_source_is_available",
        ))
        .env(CHILD, "1")
        .isolated_store(directory.path())
        .env("MJ_INSTANCE", "issue-1138-recovery")
        .env("MJ_WORKER_BINARY", directory.path().join("not-built-yet"))
        .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let directory = tempfile::tempdir().unwrap();
    let session_id = "0123456789abcdef0123456789abcdef";
    let root = directory.path().join(session_id);
    std::fs::create_dir(&root).unwrap();
    let binary = root.join("hel");
    let launch = root.join("launch.json");
    let restarted = root.join("restarted");
    std::fs::write(&binary, b"old worker").unwrap();
    std::fs::write(&launch, b"old launch schema").unwrap();
    let next_launch = directory.path().join("next-launch.json");
    std::fs::write(&next_launch, b"new launch schema").unwrap();
    let locator = targets::TargetLocator::LocalBare {
        worker_root: root.to_string_lossy().into_owned(),
    };
    let plan = WorkerRecoveryPlan {
        source_target: mj_core::state::TargetLocator::LocalBare { worker_root: root },
        target: None,
        workspace: None,
        exit_record: None,
        liveness_probe: CommandSpec::new("printf", ["dead\n"]).into(),
        binary_refresh: worker_binary_refresh_plan(&locator, session_id).unwrap(),
        launch_refresh: Some(WorkerLaunchRefreshPlan {
            expected_sha256: lower_hex(Sha256::digest(b"new launch schema")),
            installed_digest: installed_file_digest_command(
                &locator,
                &launch.to_string_lossy(),
                "identify test launch config",
            ),
            replace: CommandPlan {
                description: "install new launch schema".into(),
                commands: vec![CommandSpec::new(
                    "cp",
                    [
                        next_launch.to_string_lossy().into_owned(),
                        launch.to_string_lossy().into_owned(),
                    ],
                )],
            },
        }),
        restart: CommandPlan {
            description: "restart test worker".into(),
            commands: vec![CommandSpec::new(
                "touch",
                [restarted.to_string_lossy().into_owned()],
            )],
        }
        .into(),
    };
    let mut record = crate::controller::test_support::checkpoint_test_session(session_id);
    record.target = Some(Box::new(plan.source_target.clone()));
    crate::database::save_session(&record).unwrap();
    let error = crate::session_manager::recover_worker_controlled(
        plan.clone(),
        false,
        Some(session_id),
        &ProcessExecutor,
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("MJ_WORKER_BINARY is not a file"),
        "{error:#}"
    );
    assert_eq!(std::fs::read(&binary).unwrap(), b"old worker");
    assert_eq!(std::fs::read(&launch).unwrap(), b"old launch schema");
    assert!(!restarted.exists());

    std::fs::write(
        std::env::var_os("MJ_WORKER_BINARY").unwrap(),
        b"legacy worker",
    )
    .unwrap();
    let error = crate::session_manager::recover_worker_controlled(
        plan.clone(),
        false,
        Some(session_id),
        &ProcessExecutor,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("missing worker build stamp"));
    assert_eq!(std::fs::read(&binary).unwrap(), b"old worker");
    assert_eq!(std::fs::read(&launch).unwrap(), b"old launch schema");
    assert!(!restarted.exists());

    // The same recovery plan retries after the matching worker is installed;
    // no controller restart or replanning is needed.
    std::fs::write(
        std::env::var_os("MJ_WORKER_BINARY").unwrap(),
        stamped_worker(b"new worker"),
    )
    .unwrap();
    crate::session_manager::recover_worker_controlled(
        plan,
        false,
        Some(session_id),
        &ProcessExecutor,
    )
    .unwrap();
    assert_eq!(
        std::fs::read(binary).unwrap(),
        stamped_worker(b"new worker")
    );
    assert_eq!(std::fs::read(launch).unwrap(), b"new launch schema");
    assert!(restarted.exists());
}

/// The credential sync pushes a rotated login into a local session's staged
/// home as it does into a container session's. The launch configuration tells
/// the worker which file of its staged home holds the login. The sync compares
/// that file with the profile's, finds the profile's fresher, and the worker's
/// install writes it into the file the harness reads.
// Hard-won: 7b4cb35: Kimi login sync wrote credentials at home root instead of credentials/kimi-code.json
#[test]
#[cfg_attr(
    windows,
    ignore = "#1291: local bare fixture; a Windows controller runs no local bare sessions"
)]
fn a_rotated_login_reaches_the_staged_home_of_a_session_on_this_machine() {
    use mj_core::config::harness_authentication_marker;
    use mj_core::credentials::{
        CredentialSnapshot, SyncAction, read_credential_file, reconcile, write_credential_file,
    };

    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    for (index, kind) in HarnessKind::ALL.into_iter().enumerate() {
        let session_id = format!("{:032x}", index + 1);
        let home = directory.path().join(format!("{}-home", kind.id()));
        let profile = mj_core::config::HarnessProfile {
            enabled: true,
            environment: Default::default(),
            context_window_bytes: None,
            models: Default::default(),
            legacy_subagents: Default::default(),
            guardian_review_model: None,
            codex_profile: None,
            legacy_muse_auto_review: None,
            load_balancing_may_consume_credits: false,
            claude_permission_mode: None,
            mjolnir_guardian: true,
            aws: None,
            ..mj_core::config::HarnessProfile::new(kind, home.clone())
        };
        let canonical = profile.authentication_marker();
        let database = home.join(".data/opencode/opencode.db");
        if kind.keeps_logins_in_database() {
            write_opencode_database(&database, "sk-generation-1");
        } else if kind.api_key_variable().is_none() {
            std::fs::create_dir_all(canonical.parent().unwrap()).unwrap();
            std::fs::write(&canonical, login_bytes(kind, 1)).unwrap();
        }
        let worker_root = directory.path().join("workers").join(&session_id);
        let locator = targets::TargetLocator::LocalBare {
            worker_root: worker_root.to_string_lossy().into_owned(),
        };
        let mut session = crate::controller::test_support::checkpoint_test_session(&session_id);
        session.harness_kind = kind;
        session.last_profile = kind.id().into();
        session.target_template_id = "localhost".into();
        session.project_directory = Some(project.clone());
        session.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
            worker_root: worker_root.clone(),
        }));

        let (launch, _, target_home) = worker_launch_config(
            &session,
            &profile,
            None,
            &locator,
            LaunchWorkspace {
                session_id: &session_id,
                container: None,
                parent_worktree: None,
                owner: None,
                working_directories: &[],
            },
            &mj_core::state::TargetRuntimeSettings::from(
                &mj_core::config::TargetTemplate::LocalBare,
            ),
            &mj_core::config::Config::default(),
        )
        .unwrap();

        // The session runs from its own staged home, never the profile home,
        // and its harness is pointed there on every operating system.
        assert_eq!(launch.harness_home, PathBuf::from(&target_home), "{kind:?}");
        assert_ne!(launch.harness_home, home, "{kind:?}");
        assert_eq!(
            kind.home_from_environment(&launch.environment[kind.home_env()]),
            launch.harness_home,
            "{kind:?}"
        );
        // Where the worker's credential endpoint reads and installs, which
        // must be the file the harness reads its login from.
        let endpoint = launch
            .harness_home
            .join(launch.authentication_marker.as_deref().unwrap());
        assert_eq!(
            endpoint,
            harness_authentication_marker(kind, &launch.harness_home),
            "{kind:?}"
        );
        // Muse's staged root lies under the data directory; the rest of the
        // exchange is the same for it. DSH has no login to rotate: its key
        // comes from its environment.
        if kind.api_key_variable().is_some() {
            continue;
        }
        // OpenCode's logins live in its database: the sync pushes its export
        // into the staged home, and never copies a session's back.
        if kind.keeps_logins_in_database() {
            stage_profile(&profile, &launch.harness_home).unwrap();
            write_opencode_database(&database, "sk-generation-2");
            let export = crate::opencode_logins::profile_logins(&profile)
                .unwrap()
                .unwrap();
            let profile_copy = CredentialSnapshot::of(kind, &export);
            let (session_copy, _) = read_credential_file(kind, &endpoint).unwrap();
            let push =
                crate::worker_client::stored_logins_push(&profile_copy, &export, &session_copy)
                    .expect("the rotated login is pushed");
            write_credential_file(kind, &endpoint, push).unwrap();
            assert_eq!(std::fs::read(&endpoint).unwrap(), export);
            let (session_copy, _) = read_credential_file(kind, &endpoint).unwrap();
            assert_eq!(
                crate::worker_client::stored_logins_push(&profile_copy, &export, &session_copy),
                None
            );
            assert!(!canonical.exists(), "the profile home holds no copy");
            continue;
        }
        stage_profile(&profile, &launch.harness_home).unwrap();
        // #1490: a staged Claude login never carries the refresh token the
        // daemon alone may spend; the profile's own file keeps it.
        if kind.copies_omit_refresh_token() {
            let staged: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&endpoint).unwrap()).unwrap();
            assert!(staged["claudeAiOauth"].get("refreshToken").is_none());
            assert_eq!(staged["claudeAiOauth"]["accessToken"], "access-1");
            assert_eq!(std::fs::read(&canonical).unwrap(), login_bytes(kind, 1));
        }

        // The profile's login rotates while the session runs.
        std::fs::write(&canonical, login_bytes(kind, 2)).unwrap();
        let (profile_copy, profile_bytes) = read_credential_file(kind, &canonical).unwrap();
        let (session_copy, _) = read_credential_file(kind, &endpoint).unwrap();
        assert_eq!(
            reconcile(&profile_copy, &session_copy),
            SyncAction::Push,
            "{kind:?}"
        );
        write_credential_file(kind, &endpoint, &profile_bytes).unwrap();

        assert_eq!(
            std::fs::read(harness_authentication_marker(kind, &launch.harness_home)).unwrap(),
            login_bytes(kind, 2),
            "{kind:?}"
        );
        assert_eq!(std::fs::read(&canonical).unwrap(), login_bytes(kind, 2));
    }
}

#[test]
fn launching_where_the_role_cannot_apply_announces_why() {
    let home = tempfile::tempdir().unwrap();
    let id = "0123456789abcdef0123456789abcdef";
    let mut profile = mj_core::config::HarnessProfile::new(HarnessKind::Claude, home.path());
    profile
        .environment
        .insert("CLAUDE_CODE_USE_BEDROCK".into(), "1".into());
    profile.aws = Some(mj_core::config::AwsRoleConfig {
        role_arn: "arn:aws:iam::123456789012:role/mj-bedrock".into(),
        session_policy: None,
        session_policy_arns: Vec::new(),
        duration_seconds: None,
        source_identity: false,
    });
    let mut config = mj_core::config::Config::default();
    config.profiles.insert("bedrock".into(), profile);
    config.targets.insert(
        "localhost".into(),
        mj_core::config::TargetTemplate::LocalBare,
    );
    let mut session = crate::controller::test_support::checkpoint_test_session(id);
    session.last_profile = "bedrock".into();
    session.target_template_id = "localhost".into();
    session.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
        worker_root: format!("/home/me/.local/share/hel/workers/{id}").into(),
    }));
    let controller = Controller {
        config,
        state: mj_core::state::State {
            sessions: [(id.to_owned(), session)].into_iter().collect(),
            ..Default::default()
        },
    };

    let notice = controller.aws_role_notice(id).unwrap().unwrap();

    assert!(
        notice.contains("[profiles.bedrock.aws] has no effect on this session"),
        "{notice}"
    );
    assert!(
        notice.contains("raw local sessions read the host's own AWS files"),
        "{notice}"
    );
    // Nothing is minted or synced for a session the role cannot apply to.
    assert!(controller.aws_credential_source(id).unwrap().is_none());
}
