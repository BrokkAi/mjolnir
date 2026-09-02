use super::*;
use std::cell::RefCell;
use std::sync::{Barrier, Mutex};

const SESSION: &str = "018f9dd2-a3b4-7c8d-9000-123456789abc";

#[test]
fn process_executor_streams_stdin_and_captures_output() {
    let mut input = std::io::Cursor::new(b"streamed input".to_vec());
    let output = ProcessExecutor
        .execute_with_stdin(
            &CommandSpec::new("sh", ["-c", "cat"]).purpose("echo streamed input"),
            &mut input,
        )
        .unwrap();

    assert_eq!(output.status, 0);
    assert_eq!(output.stdout, b"streamed input");
    assert!(output.stderr.is_empty());
}

/// A child that reads its stdin to end of file, as the checkpoint export
/// worker does, only exits once the write end of the pipe is closed. Every
/// executor that streams stdin has to close it after the last chunk.
fn assert_streams_to_eof(executor: &dyn CommandExecutor) {
    let payload = vec![b'x'; 256 * 1024];
    let mut input = std::io::Cursor::new(payload.clone());
    let started = std::time::Instant::now();

    let output = executor
        .execute_with_stdin(
            &CommandSpec::new("sh", ["-c", "cat"]).purpose("echo a stream read to eof"),
            &mut input,
        )
        .unwrap();

    assert_eq!(output.status, 0);
    assert_eq!(output.stdout, payload);
    assert!(output.stderr.is_empty());
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn process_executor_completes_a_stream_against_a_child_that_reads_to_eof() {
    assert_streams_to_eof(&ProcessExecutor);
}

#[test]
fn cancellable_executor_completes_a_stream_against_a_child_that_reads_to_eof() {
    assert_streams_to_eof(&CancellableProcessExecutor::new(Arc::new(AtomicBool::new(
        false,
    ))));
}

#[cfg(unix)]
#[test]
fn cancellable_executor_drains_large_stdout_and_stderr_concurrently() {
    let output = CancellableProcessExecutor::with_timeout(Duration::from_secs(5))
            .execute(
                &CommandSpec::new(
                    "sh",
                    [
                        "-c",
                        "head -c 131072 /dev/zero | tr '\\000' x; head -c 131072 /dev/zero | tr '\\000' y >&2",
                    ],
                )
                .purpose("emit large command output"),
            )
            .unwrap();

    assert_eq!(output.status, 0);
    assert_eq!(output.stdout, vec![b'x'; 128 * 1024]);
    assert_eq!(output.stderr, vec![b'y'; 128 * 1024]);
}

fn bundle() -> ProjectBundleSpec {
    ProjectBundleSpec {
        primary: "app".to_owned(),
        repositories: vec![
            RepositorySpec {
                url: Some("git@github.com:example/app.git".to_owned()),
                destination: "app".to_owned(),
                git_ref: Some("main".to_owned()),
                reference: None,
            },
            RepositorySpec {
                url: Some("https://github.com/example/lib.git".to_owned()),
                destination: "libs/lib".to_owned(),
                git_ref: None,
                reference: None,
            },
        ],
    }
}

fn ssh() -> SshTarget {
    SshTarget {
        destination: "dev@example.test".to_owned(),
        ssh_args: vec!["-o".to_owned(), "BatchMode=yes".to_owned()],
    }
}

struct PodmanPreflightExecutor {
    seen: RefCell<Vec<CommandSpec>>,
    outputs: RefCell<Vec<CommandOutput>>,
}

impl PodmanPreflightExecutor {
    fn with_outputs(outputs: impl IntoIterator<Item = CommandOutput>) -> Self {
        Self {
            seen: RefCell::new(vec![]),
            outputs: RefCell::new(outputs.into_iter().collect()),
        }
    }
}

impl CommandExecutor for PodmanPreflightExecutor {
    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        self.seen.borrow_mut().push(command.clone());
        Ok(self.outputs.borrow_mut().remove(0))
    }
}

fn podman_output(stdout: impl AsRef<[u8]>) -> CommandOutput {
    CommandOutput {
        status: 0,
        stdout: stdout.as_ref().to_vec(),
        stderr: vec![],
    }
}

fn podman_inspection(status: &str, session_id: &str, managed: &str) -> CommandOutput {
    podman_output(
        serde_json::to_vec(&serde_json::json!([{
            "Id": "0123456789abcdef",
            "Config": {
                "Labels": {
                    (MANAGED_LABEL): managed,
                    (SESSION_LABEL): session_id,
                }
            },
            "State": { "Status": status },
        }]))
        .unwrap(),
    )
}

#[test]
fn podman_preflight_requires_supported_rootless_uid_mapped_runtime() {
    let executor = PodmanPreflightExecutor::with_outputs([
        podman_output(b"podman version 5.4.2\n"),
        podman_output(b"true\n"),
        podman_output(b"         0       1000          1\n         1     100000      65536\n"),
    ]);

    assert_eq!(
        verify_local_podman(&executor).unwrap(),
        PodmanPreflight {
            version: "5.4.2".into(),
            warnings: vec![],
        }
    );
    let seen = executor.seen.borrow();
    assert_eq!(seen.len(), 3);
    assert_eq!(seen[0].args, ["--version"]);
    assert_eq!(
        seen[1].args,
        ["info", "--format", "{{.Host.Security.Rootless}}"]
    );
    assert_eq!(seen[2].args, ["unshare", "cat", "/proc/self/uid_map"]);
}

#[test]
fn podman_preflight_guidance_names_mjolnir() {
    let executor = PodmanPreflightExecutor::with_outputs([
        podman_output(b"podman version 5.4.2\n"),
        podman_output(b"false\n"),
    ]);

    let error = verify_local_podman(&executor).unwrap_err().to_string();
    assert!(
        error.contains("Run Mjolnir as the ordinary user"),
        "{error}"
    );
    assert!(!error.contains("Run Hel"), "{error}");
}

#[test]
fn docker_preflight_requires_a_reachable_linux_daemon() {
    let ready = PodmanPreflightExecutor::with_outputs([podman_output(b"29.0.1 linux\n")]);
    assert_eq!(
        verify_local_docker(&ready).unwrap(),
        DockerPreflight {
            version: "29.0.1".into(),
        }
    );
    assert_eq!(ready.seen.borrow()[0].program, "docker");
    assert_eq!(
        ready.seen.borrow()[0].args,
        ["version", "--format", "{{.Server.Version}} {{.Server.Os}}"]
    );

    let desktop = PodmanPreflightExecutor::with_outputs([podman_output(b"29.0.1 windows\n")]);
    let error = verify_local_docker(&desktop).unwrap_err().to_string();
    assert!(error.contains("expected a Linux Docker daemon"), "{error}");
    assert!(error.contains(DOCKER_DOCUMENTATION_PATH), "{error}");

    let unavailable = PodmanPreflightExecutor::with_outputs([CommandOutput {
        status: 1,
        stdout: vec![],
        stderr: b"daemon unavailable".to_vec(),
    }]);
    let error = verify_local_docker(&unavailable).unwrap_err().to_string();
    assert!(error.contains("user running Mjolnir"), "{error}");
    assert!(!error.contains("user running Hel"), "{error}");
}

#[test]
fn podman_preflight_rejects_unsupported_version_with_upgrade_remediation() {
    let executor =
        PodmanPreflightExecutor::with_outputs([podman_output(b"podman version 3.4.7\n")]);

    let error = verify_local_podman(&executor).unwrap_err().to_string();
    assert!(error.contains("Podman 4.0.0 or newer"));
    assert!(error.contains("apt install -y podman uidmap"));
    assert!(error.contains(PODMAN_DOCUMENTATION_PATH));
}

#[test]
fn podman_preflight_reports_uidmap_helper_remediation() {
    let executor = PodmanPreflightExecutor::with_outputs([
        podman_output(b"podman version 5.4.2\n"),
        podman_output(b"true\n"),
        CommandOutput {
            status: 1,
            stdout: vec![],
            stderr: b"cannot find newuidmap executable".to_vec(),
        },
    ]);

    let error = verify_local_podman(&executor).unwrap_err().to_string();
    assert!(error.contains("podman unshare cat /proc/self/uid_map"));
    assert!(error.contains("apt install -y uidmap"));
    assert!(error.contains(PODMAN_DOCUMENTATION_PATH));
}

#[test]
fn podman_preflight_rejects_a_uid_map_without_subordinate_ids() {
    let executor = PodmanPreflightExecutor::with_outputs([
        podman_output(b"podman version 5.4.2\n"),
        podman_output(b"true\n"),
        podman_output(b"         0       1000          1\n"),
    ]);

    let error = verify_local_podman(&executor).unwrap_err().to_string();
    assert!(error.contains("maps container UIDs 0 and 1"));
    assert!(error.contains("usermod --add-subuids"));
    assert!(error.contains(PODMAN_DOCUMENTATION_PATH));
}

#[test]
fn ssh_podman_preflight_runs_every_probe_through_noninteractive_ssh() {
    let executor = PodmanPreflightExecutor::with_outputs([
        podman_output(b"podman version 5.4.2\n"),
        podman_output(b"true\n"),
        podman_output(b"         0       1000          1\n         1     100000      65536\n"),
        podman_output(b"yes\n"),
    ]);

    let preflight = verify_ssh_podman(&ssh(), &executor).unwrap();

    assert_eq!(preflight.version, "5.4.2");
    let seen = executor.seen.borrow();
    assert_eq!(seen.len(), 4);
    for command in seen.iter() {
        assert_eq!(command.program, "ssh");
        assert!(command.args.contains(&"BatchMode=yes".to_owned()));
        assert!(command.args.contains(&"ConnectTimeout=3".to_owned()));
        assert!(command.args.contains(&"dev@example.test".to_owned()));
    }
    assert!(
        seen[2]
            .args
            .last()
            .unwrap()
            .contains("'/proc/self/uid_map'")
    );
    assert!(seen[3].args.last().unwrap().contains("'loginctl show-user"));
    assert!(seen[3].args.last().unwrap().contains("'sh' '-c'"));
    assert!(!seen[3].args.last().unwrap().contains("'-lc'"));
    assert!(preflight.warnings.is_empty());
}

#[test]
fn ssh_podman_preflight_warns_when_remote_user_lingering_is_disabled() {
    let executor = PodmanPreflightExecutor::with_outputs([
        podman_output(b"podman version 5.4.2\n"),
        podman_output(b"true\n"),
        podman_output(b"         0       1000          1\n         1     100000      65536\n"),
        podman_output(b"no\n"),
    ]);

    let preflight = verify_ssh_podman(&ssh(), &executor).unwrap();

    assert_eq!(preflight.warnings.len(), 1);
    let warning = &preflight.warnings[0];
    assert!(
        warning
            .detail
            .contains("lingering is disabled on dev@example.test")
    );
    assert!(warning.detail.contains("last SSH connection closes"));
    assert!(warning.remediation.contains("sudo loginctl enable-linger"));
}

#[test]
fn ssh_podman_preflight_warns_when_linger_check_is_unavailable() {
    let executor = PodmanPreflightExecutor::with_outputs([
        podman_output(b"podman version 5.4.2\n"),
        podman_output(b"true\n"),
        podman_output(b"         0       1000          1\n         1     100000      65536\n"),
        CommandOutput {
            status: 127,
            stdout: vec![],
            stderr: b"sh: loginctl: not found\n".to_vec(),
        },
    ]);

    let preflight = verify_ssh_podman(&ssh(), &executor).unwrap();

    assert_eq!(preflight.warnings.len(), 1);
    let warning = &preflight.warnings[0];
    assert!(warning.detail.contains("durability check is unavailable"));
    assert!(warning.detail.contains("`loginctl` was not found"));
    assert!(warning.detail.contains("may not use systemd"));
    assert!(warning.detail.contains("Mjolnir cannot verify"));
    assert!(!warning.detail.contains("Hel"));
    assert!(warning.remediation.contains("service manager"));
}

#[test]
fn ssh_podman_preflight_failures_name_the_destination_and_remote_scope() {
    let executor =
        PodmanPreflightExecutor::with_outputs([podman_output(b"podman version 3.4.7\n")]);

    let error = verify_ssh_podman(&ssh(), &executor)
        .unwrap_err()
        .to_string();

    assert!(error.contains("Remote Podman preflight failed on dev@example.test"));
    assert!(error.contains("On dev@example.test: Upgrade Podman"));
    assert!(error.contains(PODMAN_DOCUMENTATION_PATH));
}

#[test]
fn ssh_podman_preflight_reports_an_unreachable_host_separately_from_podman() {
    let executor = PodmanPreflightExecutor::with_outputs([CommandOutput {
        status: 255,
        stdout: vec![],
        stderr: b"ssh: connect to host example.test port 22: Connection timed out".to_vec(),
    }]);

    let error = verify_ssh_podman(&ssh(), &executor)
        .unwrap_err()
        .to_string();

    assert!(error.contains("SSH could not run the probes on dev@example.test"));
    assert!(error.contains("Connection timed out"));
    assert!(!error.contains("Podman 4.0.0"));
}

#[test]
fn ssh_podman_preflight_rejects_an_unusable_destination_without_running_ssh() {
    let executor = PodmanPreflightExecutor::with_outputs([]);
    let target = SshTarget {
        destination: "--oProxyCommand=touch /tmp/pwn".to_owned(),
        ssh_args: vec![],
    };

    let error = verify_ssh_podman(&target, &executor)
        .unwrap_err()
        .to_string();

    assert!(error.contains("SSH destination is unusable"));
    assert!(executor.seen.borrow().is_empty());
}

#[test]
fn setup_smoke_plan_wraps_every_ssh_podman_command_in_ssh() {
    let plan = setup_smoke_plan(
        &TargetTemplate::SshPodman {
            ssh: ssh(),
            container: ContainerTemplate {
                image: "ubuntu:24.04".to_owned(),
                pull_policy: ImagePullPolicy::Auto,
                extra_run_args: vec![],
            },
        },
        "setup-123",
    )
    .unwrap();

    assert_eq!(plan.commands.len(), 3);
    for command in &plan.commands {
        assert_eq!(command.program, "ssh");
        assert!(command.args.contains(&"dev@example.test".to_owned()));
        assert!(command.args.last().unwrap().starts_with("'podman'"));
    }
    assert!(
        plan.commands[0]
            .args
            .last()
            .unwrap()
            .contains("'run' '--init'")
    );
    assert!(plan.commands[1].args.last().unwrap().ends_with("'true'"));
    assert!(
        plan.commands[2]
            .args
            .last()
            .unwrap()
            .contains("'rm' '--force'")
    );
    assert_eq!(
        plan.commands[2].purpose,
        "remove disposable setup container"
    );
}

#[test]
fn managed_resource_identity_args_build_container_labels_and_ec2_tags() {
    assert_eq!(
        managed_resource_identity_args(ManagedResourceKind::Container, SESSION),
        vec![
            "--label",
            "dev.mj.session=018f9dd2-a3b4-7c8d-9000-123456789abc",
            "--label",
            "dev.mj.managed=true",
        ]
    );
    assert_eq!(
        managed_resource_identity_args(ManagedResourceKind::Ec2Instance, SESSION),
        vec![
            "--tag-specifications",
            "ResourceType=instance,Tags=[{Key=dev.mj.session,Value=018f9dd2-a3b4-7c8d-9000-123456789abc},{Key=dev.mj.managed,Value=true}]",
        ]
    );
}

#[test]
fn container_template_ownership_errors_name_mjolnir() {
    let template = ContainerTemplate {
        image: "ubuntu:24.04".to_owned(),
        pull_policy: ImagePullPolicy::Auto,
        extra_run_args: vec!["--label=dev.mj.managed=false".to_owned()],
    };

    let error = validate_container_template(&template)
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "container template may not override Mjolnir ownership labels"
    );
}

#[test]
fn podman_target_recovery_uses_the_exact_local_or_remote_container() {
    let name = resource_name(SESSION).unwrap();
    let local = target_recovery_plan(
        &TargetLocator::LocalPodman {
            container_id: name.clone(),
        },
        SESSION,
    )
    .unwrap()
    .unwrap();
    assert_eq!(local.exists.args, ["container", "exists", name.as_str()]);
    assert_eq!(local.inspect.args, ["container", "inspect", name.as_str()]);
    assert_eq!(local.start.args, ["start", name.as_str()]);

    let remote = target_recovery_plan(
        &TargetLocator::SshPodman {
            ssh: ssh(),
            container_id: name.clone(),
        },
        SESSION,
    )
    .unwrap()
    .unwrap();
    assert_eq!(remote.exists.program, "ssh");
    assert_eq!(
        remote.exists.args.last().unwrap(),
        &format!("'podman' 'container' 'exists' '{name}'")
    );
    assert_eq!(remote.inspect.program, "ssh");
    assert_eq!(
        remote.inspect.args.last().unwrap(),
        &format!("'podman' 'container' 'inspect' '{name}'")
    );
    assert_eq!(
        remote.start.args.last().unwrap(),
        &format!("'podman' 'start' '{name}'")
    );
}

#[test]
fn stopped_owned_podman_target_is_started_and_reinspected() {
    let name = resource_name(SESSION).unwrap();
    let plan =
        target_recovery_plan(&TargetLocator::LocalPodman { container_id: name }, SESSION).unwrap();
    let executor = PodmanPreflightExecutor::with_outputs([
        podman_output(""),
        podman_inspection("exited", SESSION, "true"),
        podman_output("container-id\n"),
        podman_inspection("running", SESSION, "true"),
    ]);

    assert_eq!(
        ensure_recovery_target_running(&executor, plan.as_ref()).unwrap(),
        TargetRecoveryOutcome::Started
    );
    let seen = executor.seen.borrow();
    assert_eq!(seen.len(), 4);
    assert_eq!(seen[0].purpose, "check for Mjolnir session container");
    assert_eq!(seen[1].purpose, "inspect Mjolnir session container");
    assert_eq!(seen[2].purpose, "start stopped Mjolnir session container");
    assert_eq!(seen[3].purpose, "inspect Mjolnir session container");
}

#[test]
fn running_podman_target_is_not_started() {
    let plan = TargetRecoveryPlan {
        exists: CommandSpec::new("exists", std::iter::empty::<&str>()),
        inspect: CommandSpec::new("inspect", std::iter::empty::<&str>()),
        start: CommandSpec::new("start", std::iter::empty::<&str>()),
        session_id: SESSION.into(),
    };
    let executor = PodmanPreflightExecutor::with_outputs([
        podman_output(""),
        podman_inspection("running", SESSION, "true"),
    ]);

    assert_eq!(
        ensure_recovery_target_running(&executor, Some(&plan)).unwrap(),
        TargetRecoveryOutcome::AlreadyRunning
    );
    assert_eq!(executor.seen.borrow().len(), 2);
}

#[test]
fn missing_podman_target_is_reported_without_inspection_or_start() {
    let plan = TargetRecoveryPlan {
        exists: CommandSpec::new("exists", std::iter::empty::<&str>()),
        inspect: CommandSpec::new("inspect", std::iter::empty::<&str>()),
        start: CommandSpec::new("start", std::iter::empty::<&str>()),
        session_id: SESSION.into(),
    };
    let executor = PodmanPreflightExecutor::with_outputs([CommandOutput {
        status: 1,
        stdout: Vec::new(),
        stderr: Vec::new(),
    }]);

    assert_eq!(
        ensure_recovery_target_running(&executor, Some(&plan)).unwrap(),
        TargetRecoveryOutcome::Missing
    );
    assert_eq!(executor.seen.borrow().len(), 1);
}

#[test]
fn unsafe_podman_target_states_and_ownership_never_start() {
    for (status, session, managed, expected) in [
        ("paused", SESSION, "true", "paused"),
        ("stopping", SESSION, "true", "stopping"),
        ("exited", "another-session", "true", "another session"),
        ("exited", SESSION, "false", "Mjolnir does not own"),
    ] {
        let plan = TargetRecoveryPlan {
            exists: CommandSpec::new("exists", std::iter::empty::<&str>()),
            inspect: CommandSpec::new("inspect", std::iter::empty::<&str>()),
            start: CommandSpec::new("start", std::iter::empty::<&str>()),
            session_id: SESSION.into(),
        };
        let executor = PodmanPreflightExecutor::with_outputs([
            podman_output(""),
            podman_inspection(status, session, managed),
        ]);

        let error = ensure_recovery_target_running(&executor, Some(&plan))
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "{error}");
        assert_eq!(executor.seen.borrow().len(), 2);
    }
}

#[test]
fn podman_target_must_still_be_running_after_start() {
    let plan = TargetRecoveryPlan {
        exists: CommandSpec::new("exists", std::iter::empty::<&str>()),
        inspect: CommandSpec::new("inspect", std::iter::empty::<&str>()),
        start: CommandSpec::new("start", std::iter::empty::<&str>()),
        session_id: SESSION.into(),
    };
    let executor = PodmanPreflightExecutor::with_outputs([
        podman_output(""),
        podman_inspection("exited", SESSION, "true"),
        podman_output("container-id\n"),
        podman_inspection("exited", SESSION, "true"),
    ]);

    let error = ensure_recovery_target_running(&executor, Some(&plan))
        .unwrap_err()
        .to_string();
    assert!(error.contains("after start"), "{error}");
}

#[test]
fn podman_inspect_or_start_failures_stop_recovery() {
    let plan = TargetRecoveryPlan {
        exists: CommandSpec::new("exists", std::iter::empty::<&str>()).purpose("check test target"),
        inspect: CommandSpec::new("inspect", std::iter::empty::<&str>())
            .purpose("inspect test target"),
        start: CommandSpec::new("start", std::iter::empty::<&str>()).purpose("start test target"),
        session_id: SESSION.into(),
    };
    let inspect_failed = PodmanPreflightExecutor::with_outputs([
        podman_output(""),
        CommandOutput {
            status: 125,
            stdout: Vec::new(),
            stderr: b"storage unavailable".to_vec(),
        },
    ]);
    let error = ensure_recovery_target_running(&inspect_failed, Some(&plan))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("inspect container session target"),
        "{error}"
    );
    assert_eq!(inspect_failed.seen.borrow().len(), 2);

    let start_failed = PodmanPreflightExecutor::with_outputs([
        podman_output(""),
        podman_inspection("exited", SESSION, "true"),
        CommandOutput {
            status: 1,
            stdout: Vec::new(),
            stderr: b"start refused".to_vec(),
        },
    ]);
    let error = ensure_recovery_target_running(&start_failed, Some(&plan))
        .unwrap_err()
        .to_string();
    assert!(error.contains("start confirmed stopped"), "{error}");
    assert_eq!(start_failed.seen.borrow().len(), 3);
}

#[test]
fn podman_plan_uses_owned_name_label_and_argv_clones() {
    let plan = provision_plan(
        &TargetTemplate::LocalPodman(ContainerTemplate {
            image: "ubuntu:24.04".to_owned(),
            pull_policy: ImagePullPolicy::Auto,
            extra_run_args: vec!["--cpus=4".to_owned()],
        }),
        SESSION,
        &bundle(),
        &[],
    )
    .unwrap();
    let name = resource_name(SESSION).unwrap();
    assert_eq!(plan.commands[0].program, "podman");
    assert!(
        plan.commands[0]
            .args
            .windows(2)
            .any(|args| args == ["--name", &name])
    );
    assert!(plan.commands[0].args.windows(4).any(
        |args| args == managed_resource_identity_args(ManagedResourceKind::Container, SESSION)
    ));
    let clone = plan
        .commands
        .iter()
        .find(|command| command.purpose == "clone app")
        .unwrap();
    assert_eq!(&clone.args[..4], ["exec", "-i", &name, "git"]);
    assert!(clone.args.contains(&"--".to_owned()));
    assert!(clone.args.contains(&"/workspace/app".to_owned()));
    let bootstrap = plan
        .commands
        .iter()
        .find(|command| command.purpose == "install Git")
        .unwrap();
    assert!(bootstrap.args.last().unwrap().contains("command -v git"));
    assert!(bootstrap.args.last().unwrap().contains("command -v gh"));
    assert!(
        bootstrap
            .args
            .last()
            .unwrap()
            .contains("gh auth git-credential")
    );
}

#[test]
fn container_clone_borrows_from_an_optional_read_only_reference() {
    let mut cached = bundle();
    cached.repositories[0].reference = Some("/run/hel/git-cache/app.git".to_owned());
    let plan = provision_plan(
        &TargetTemplate::AppleContainer(ContainerTemplate {
            image: "ubuntu:24.04".to_owned(),
            pull_policy: ImagePullPolicy::Auto,
            extra_run_args: vec![],
        }),
        SESSION,
        &cached,
        &[],
    )
    .unwrap();

    let clone = plan
        .commands
        .iter()
        .find(|command| command.purpose == "clone app")
        .unwrap();
    assert!(
        clone.args.windows(2).any(|arguments| {
            arguments == ["--reference-if-able", "/run/hel/git-cache/app.git"]
        })
    );
    assert!(clone.args.contains(&"--branch".to_owned()));
    assert!(
        clone
            .args
            .contains(&"git@github.com:example/app.git".to_owned())
    );
}

#[test]
fn container_secret_is_streamed_without_entering_local_command_arguments() {
    let secret = "github-token-that-must-not-reach-argv";
    let target = TargetTemplate::LocalPodman(ContainerTemplate {
        image: "ubuntu:24.04".to_owned(),
        pull_policy: ImagePullPolicy::Auto,
        extra_run_args: vec!["--env".to_owned(), "GH_TOKEN".to_owned()],
    });
    let mut plan = CommandPlan {
        description: "exercise secret launcher".to_owned(),
        commands: vec![
            CommandSpec::new("sh", ["-c", "printf %s \"$GH_TOKEN\""])
                .purpose("read inherited secret")
                .creates_target(),
        ],
    };

    plan.provide_target_environment_secret(&target, "GH_TOKEN", secret)
        .unwrap();

    let command = &plan.commands[0];
    assert_eq!(command.program, "sh");
    assert!(
        !command
            .args
            .iter()
            .any(|argument| argument.contains(secret))
    );
    assert!(!format!("{command:?}").contains(secret));
    assert!(format!("{command:?}").contains("<redacted>"));
    assert!(!serde_json::to_string(command).unwrap().contains(secret));
    let output = plan.execute(&ProcessExecutor).unwrap();
    assert_eq!(output[0].stdout, secret.as_bytes());
}

#[test]
fn container_secret_is_streamed_without_entering_remote_ssh_arguments() {
    let secret = "remote-github-token-that-must-not-reach-argv";
    let target = TargetTemplate::SshPodman {
        ssh: ssh(),
        container: ContainerTemplate {
            image: "ubuntu:24.04".to_owned(),
            pull_policy: ImagePullPolicy::Auto,
            extra_run_args: vec!["--env".to_owned(), "GH_TOKEN".to_owned()],
        },
    };
    let mut plan = provision_plan(&target, SESSION, &bundle(), &[]).unwrap();

    plan.provide_target_environment_secret(&target, "GH_TOKEN", secret)
        .unwrap();

    let command = plan
        .commands
        .iter()
        .find(|command| command.creates_target)
        .unwrap();
    assert_eq!(command.program, "ssh");
    assert!(command.args.last().unwrap().contains("read -r GH_TOKEN"));
    assert!(command.args.last().unwrap().contains("'--env' 'GH_TOKEN'"));
    assert!(
        !command
            .args
            .iter()
            .any(|argument| argument.contains(secret))
    );
    assert!(!format!("{command:?}").contains(secret));
    assert!(format!("{command:?}").contains("<redacted>"));
    assert!(!serde_json::to_string(command).unwrap().contains(secret));
}

#[test]
fn podman_plan_only_marks_per_repository_clone_commands_for_parallel_execution() {
    let plan = provision_plan(
        &TargetTemplate::LocalPodman(ContainerTemplate {
            image: "ubuntu:24.04".to_owned(),
            pull_policy: ImagePullPolicy::Auto,
            extra_run_args: vec![],
        }),
        SESSION,
        &bundle(),
        &[],
    )
    .unwrap();

    let bootstrap = plan
        .commands
        .iter()
        .find(|command| command.purpose == "install Git")
        .unwrap();
    assert_eq!(bootstrap.parallel_group, None);

    let mkdir = plan
        .commands
        .iter()
        .find(|command| command.purpose == "create bundle workspace")
        .unwrap();
    assert_eq!(mkdir.parallel_group, None);

    let clone_app = plan
        .commands
        .iter()
        .find(|command| command.purpose == "clone app")
        .unwrap();
    let clone_lib = plan
        .commands
        .iter()
        .find(|command| command.purpose == "clone libs/lib")
        .unwrap();
    assert!(clone_app.parallel_group.is_some());
    assert_eq!(clone_app.parallel_group, clone_lib.parallel_group);
}

#[test]
fn podman_containers_reap_zombies_and_apple_containers_keep_their_defaults() {
    let podman = provision_plan(
        &TargetTemplate::LocalPodman(ContainerTemplate {
            image: "ubuntu:24.04".to_owned(),
            pull_policy: ImagePullPolicy::Auto,
            extra_run_args: vec![],
        }),
        SESSION,
        &bundle(),
        &[],
    )
    .unwrap();
    assert_eq!(podman.commands[0].args[0], "run");
    assert_eq!(podman.commands[0].args[1], "--init");

    let remote = provision_plan(
        &TargetTemplate::SshPodman {
            ssh: ssh(),
            container: ContainerTemplate {
                image: "dev:1".to_owned(),
                pull_policy: ImagePullPolicy::Auto,
                extra_run_args: vec![],
            },
        },
        SESSION,
        &bundle(),
        &[],
    )
    .unwrap();
    assert!(
        remote.commands[0]
            .args
            .last()
            .unwrap()
            .contains("'podman' 'run' '--init'")
    );

    let apple = provision_plan(
        &TargetTemplate::AppleContainer(ContainerTemplate {
            image: "ubuntu:24.04".to_owned(),
            pull_policy: ImagePullPolicy::Auto,
            extra_run_args: vec![],
        }),
        SESSION,
        &bundle(),
        &[],
    )
    .unwrap();
    assert_eq!(apple.commands[1].args[0], "run");
    assert!(!apple.commands[1].args.contains(&"--init".to_owned()));
}

/// A launch never waits on a registry under the default policy. The daemon
/// refreshes remote `:latest` images in the background instead, so a session
/// starts from whatever the host already has.
#[test]
fn an_automatic_pull_policy_never_pulls_during_a_launch() {
    for engine in ["podman", "docker"] {
        for image in [
            "ghcr.io/example/dev:latest",
            "ghcr.io/example/dev",
            "ghcr.io/example/dev:1.2.3",
            "localhost/example/dev:latest",
            "local/example:latest",
            "ghcr.io/example/dev@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            let args = container_run_args(
                engine,
                &ContainerTemplate {
                    image: image.to_owned(),
                    pull_policy: ImagePullPolicy::Auto,
                    extra_run_args: vec![],
                },
                "container-name",
                SESSION,
                &[],
            )
            .unwrap();
            let pull = args
                .iter()
                .find(|argument| argument.starts_with("--pull="))
                .map(String::as_str);
            // Podman spells "use the cached image" as no flag at all; Docker
            // needs the flag.
            let expected = if engine == "podman" {
                None
            } else {
                Some("--pull=missing")
            };
            assert_eq!(
                pull, expected,
                "unexpected {engine} pull policy for {image}"
            );
        }
    }
}

/// A configured policy is a decision about launches, and it survives.
#[test]
fn an_explicit_newer_pull_policy_still_pulls_during_a_launch() {
    for image in ["ghcr.io/example/dev:latest", "ghcr.io/example/dev:1.2.3"] {
        let args = container_run_args(
            "podman",
            &ContainerTemplate {
                image: image.to_owned(),
                pull_policy: ImagePullPolicy::Newer,
                extra_run_args: vec![],
            },
            "container-name",
            SESSION,
            &[],
        )
        .unwrap();
        assert!(
            args.contains(&"--pull=newer".to_owned()),
            "explicit newer policy lost for {image}: {args:?}"
        );
    }
}

#[test]
fn explicit_podman_pull_policy_overrides_image_tag_defaults() {
    for (policy, expected) in [
        (ImagePullPolicy::Always, "--pull=always"),
        (ImagePullPolicy::Newer, "--pull=newer"),
        (ImagePullPolicy::Missing, ""),
        (ImagePullPolicy::Never, "--pull=never"),
    ] {
        let args = container_run_args(
            "podman",
            &ContainerTemplate {
                image: "ghcr.io/example/dev:1.2.3".to_owned(),
                pull_policy: policy,
                extra_run_args: vec![],
            },
            "container-name",
            SESSION,
            &[],
        )
        .unwrap();
        assert_eq!(
            args.iter()
                .find(|argument| argument.starts_with("--pull="))
                .map(String::as_str)
                .unwrap_or_default(),
            expected
        );
    }
}

#[test]
fn docker_pull_policy_uses_supported_digest_aware_run_modes() {
    for (policy, expected) in [
        (ImagePullPolicy::Always, "--pull=always"),
        (ImagePullPolicy::Newer, "--pull=always"),
        (ImagePullPolicy::Missing, "--pull=missing"),
        (ImagePullPolicy::Never, "--pull=never"),
    ] {
        let args = container_run_args(
            "docker",
            &ContainerTemplate {
                image: "ghcr.io/example/dev:1.2.3".to_owned(),
                pull_policy: policy,
                extra_run_args: vec![],
            },
            "container-name",
            SESSION,
            &[],
        )
        .unwrap();
        assert!(args.contains(&expected.to_owned()), "{args:?}");
        assert!(!args.contains(&"--pull=newer".to_owned()), "{args:?}");
    }
}

#[test]
fn podman_additional_mounts_use_copy_on_write_overlay_volumes() {
    let mounts = [
        AdditionalMount {
            source: PathBuf::from("/host/cache"),
            destination: PathBuf::from("/mnt/cache"),
            read_only: false,
        },
        AdditionalMount {
            source: PathBuf::from("/host/models"),
            destination: PathBuf::from("/mnt/models"),
            read_only: true,
        },
    ];
    let plan = provision_plan(
        &TargetTemplate::LocalPodman(ContainerTemplate {
            image: "ubuntu:24.04".to_owned(),
            pull_policy: ImagePullPolicy::Auto,
            extra_run_args: vec![],
        }),
        SESSION,
        &bundle(),
        &mounts,
    )
    .unwrap();

    assert!(
        plan.commands[0]
            .args
            .windows(2)
            .any(|args| args == ["--volume", "/host/cache:/mnt/cache:O"])
    );
    assert!(
        plan.commands[0]
            .args
            .windows(2)
            .any(|args| args == ["--volume", "/host/models:/mnt/models:ro"])
    );
}

#[test]
fn docker_additional_mounts_use_managed_overlay_and_read_only_bind_volumes() {
    let mounts = [
        AdditionalMount {
            source: PathBuf::from("/host/cache"),
            destination: PathBuf::from("/mnt/cache"),
            read_only: false,
        },
        AdditionalMount {
            source: PathBuf::from("/host/models"),
            destination: PathBuf::from("/mnt/models"),
            read_only: true,
        },
    ];
    let plan = provision_plan(
        &TargetTemplate::LocalDocker(ContainerTemplate {
            image: "ubuntu:24.04".to_owned(),
            pull_policy: ImagePullPolicy::Auto,
            extra_run_args: vec![],
        }),
        SESSION,
        &bundle(),
        &mounts,
    )
    .unwrap();

    let create = &plan.commands[0];
    let name = resource_name(SESSION).unwrap();
    let volume = format!("{name}-mount-0");
    assert_eq!(create.program, "sh");
    assert!(create.creates_target);
    assert!(create.args[1].contains("docker volume create"));
    assert!(create.args[1].contains("--opt type=overlay"));
    assert!(create.args[1].contains("lowerdir=$source,upperdir=$upper,workdir=$work"));
    assert!(create.args[1].contains("refusing foreign Docker volume"));
    assert!(create.args.contains(&"/host/cache".to_owned()));
    assert!(create.args.contains(&volume));
    assert!(
        create
            .args
            .windows(2)
            .any(|args| { args == ["--volume", format!("{volume}:/mnt/cache").as_str()] })
    );
    assert!(
        create
            .args
            .windows(2)
            .any(|args| args == ["--volume", "/host/models:/mnt/models:ro"])
    );
    assert!(create.args.contains(&"--pull=missing".to_owned()));
    assert!(create.args.contains(&"--init".to_owned()));

    let clone = plan
        .commands
        .iter()
        .find(|command| command.purpose == "clone app")
        .unwrap();
    assert_eq!(clone.program, "docker");
    assert_eq!(&clone.args[..4], ["exec", "-i", name.as_str(), "git"]);
}

#[test]
fn overlay_denylist_covers_network_fuse_and_metadata_poor_filesystems() {
    assert_eq!(
        overlay_unsupported_filesystem("nfs"),
        Some("network filesystem")
    );
    assert_eq!(
        overlay_unsupported_filesystem("  NFS4 "),
        Some("network filesystem")
    );
    // FUSE names its backing driver, and the case comes from the kernel.
    assert_eq!(
        overlay_unsupported_filesystem("FUSE.sshfs"),
        Some("FUSE filesystem")
    );
    assert_eq!(
        overlay_unsupported_filesystem("fuseblk"),
        Some("FUSE filesystem")
    );
    assert_eq!(
        overlay_unsupported_filesystem("exfat"),
        Some("no POSIX metadata")
    );
    assert_eq!(
        overlay_unsupported_filesystem("overlayfs"),
        Some("overlay stacking limit")
    );
    // Anything else, known-good or unrecognized, keeps the overlay.
    assert_eq!(overlay_unsupported_filesystem("ext4"), None);
    assert_eq!(overlay_unsupported_filesystem("btrfs"), None);
    assert_eq!(overlay_unsupported_filesystem("futurefs"), None);
    assert_eq!(overlay_unsupported_filesystem(""), None);
}

#[test]
fn filesystem_probe_answers_positionally_and_reaches_the_podman_host() {
    let executor = PodmanPreflightExecutor::with_outputs([podman_output(b"ext4\nnfs\n")]);
    let paths = [PathBuf::from("/host/cache"), PathBuf::from("/host/models")];

    assert_eq!(
        probe_filesystem_types(None, &paths, &executor).unwrap(),
        vec!["ext4".to_owned(), "nfs".to_owned()]
    );
    let seen = executor.seen.borrow();
    assert_eq!(seen[0].program, "stat");
    assert_eq!(
        seen[0].args,
        ["-f", "-c", "%T", "--", "/host/cache", "/host/models"]
    );

    let remote = PodmanPreflightExecutor::with_outputs([podman_output(b"ext4\nnfs\n")]);
    probe_filesystem_types(Some(&ssh()), &paths, &remote).unwrap();
    let seen = remote.seen.borrow();
    assert_eq!(seen[0].program, "ssh");
    assert!(
        seen[0].args.last().is_some_and(|remote| {
            remote == "'stat' '-f' '-c' '%T' '--' '/host/cache' '/host/models'"
        }),
        "{:?}",
        seen[0].args
    );
}

#[test]
fn filesystem_probe_rejects_a_partial_or_failed_answer() {
    let short = PodmanPreflightExecutor::with_outputs([podman_output(b"ext4\n")]);
    let error = probe_filesystem_types(
        None,
        &[PathBuf::from("/host/cache"), PathBuf::from("/host/models")],
        &short,
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("named 1 filesystems for 2 directories"),
        "{error}"
    );

    let failed = PodmanPreflightExecutor::with_outputs([CommandOutput {
        status: 1,
        stdout: Vec::new(),
        stderr: b"stat: cannot read file system information".to_vec(),
    }]);
    let error = probe_filesystem_types(None, &[PathBuf::from("/host/cache")], &failed)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("cannot read file system information"),
        "{error}"
    );
}

#[test]
fn apple_additional_mounts_use_read_only_bind_fallback() {
    let mounts = [AdditionalMount {
        source: PathBuf::from("/Users/me/assets"),
        destination: PathBuf::from("/mnt/assets"),
        read_only: false,
    }];
    let plan = provision_plan(
        &TargetTemplate::AppleContainer(ContainerTemplate {
            image: "ubuntu:24.04".to_owned(),
            pull_policy: ImagePullPolicy::Auto,
            extra_run_args: vec![],
        }),
        SESSION,
        &bundle(),
        &mounts,
    )
    .unwrap();

    assert!(plan.commands[1].args.windows(2).any(|args| {
        args == [
            "--mount",
            "type=bind,source=/Users/me/assets,target=/mnt/assets,readonly",
        ]
    }));
}

#[test]
fn apple_plan_preflights_and_uses_container_cli() {
    let plan = provision_plan(
        &TargetTemplate::AppleContainer(ContainerTemplate {
            image: "ghcr.io/example/dev:latest".to_owned(),
            pull_policy: ImagePullPolicy::Auto,
            extra_run_args: vec![],
        }),
        SESSION,
        &bundle(),
        &[],
    )
    .unwrap();
    assert_eq!(
        plan.commands[0],
        CommandSpec::new("container", ["system", "status"])
            .purpose("check Apple container service")
            .stage(ProvisionStage::Provisioning)
    );
    assert_eq!(
        plan.commands[1].args,
        ["image", "pull", "ghcr.io/example/dev:latest"]
    );
    let name = resource_name(SESSION).unwrap();
    assert!(
        plan.commands[2]
            .args
            .windows(2)
            .any(|args| args == ["--name", &name])
    );
    assert!(plan.commands[2].args.windows(4).any(|args| {
        args == managed_resource_identity_args(ManagedResourceKind::Container, SESSION)
    }));
}

#[test]
fn apple_pull_policy_prepares_mutable_and_pinned_images() {
    for (policy, expected_args) in [
        (
            ImagePullPolicy::Always,
            Some(vec!["image", "pull", "ghcr.io/example/dev:1"]),
        ),
        (
            ImagePullPolicy::Newer,
            Some(vec!["image", "pull", "ghcr.io/example/dev:1"]),
        ),
        (
            ImagePullPolicy::Never,
            Some(vec!["image", "inspect", "ghcr.io/example/dev:1"]),
        ),
        (ImagePullPolicy::Missing, None),
    ] {
        let commands = apple_image_prepare_commands(&ContainerTemplate {
            image: "ghcr.io/example/dev:1".to_owned(),
            pull_policy: policy,
            extra_run_args: vec![],
        });
        assert_eq!(
            commands
                .first()
                .map(|command| { command.args.iter().map(String::as_str).collect::<Vec<_>>() }),
            expected_args
        );
    }
}

#[test]
fn apple_cleanup_confirms_absence_by_the_exact_provisioned_container_id() {
    let container_id = resource_name(SESSION).unwrap();
    let locator = TargetLocator::AppleContainer {
        container_id: container_id.clone(),
    };
    let still_live = PodmanPreflightExecutor::with_outputs([podman_output(format!(
        "unrelated-id\n{container_id}\n"
    ))]);

    assert!(!cleanup_target_is_confirmed_absent(&locator, SESSION, &still_live).unwrap());
    assert_eq!(
        still_live.seen.borrow()[0].args,
        ["list", "--all", "--quiet"]
    );

    let absent = PodmanPreflightExecutor::with_outputs([podman_output("unrelated-id\n")]);
    assert!(cleanup_target_is_confirmed_absent(&locator, SESSION, &absent).unwrap());

    let failed = PodmanPreflightExecutor::with_outputs([CommandOutput {
        status: 1,
        stdout: Vec::new(),
        stderr: b"service unavailable".to_vec(),
    }]);
    assert!(cleanup_target_is_confirmed_absent(&locator, SESSION, &failed).is_err());
}

#[test]
fn docker_cleanup_confirmation_distinguishes_live_absent_and_unreachable() {
    let locator = TargetLocator::LocalDocker {
        container_id: resource_name(SESSION).unwrap(),
    };
    let live = PodmanPreflightExecutor::with_outputs([CommandOutput {
        status: 1,
        stdout: vec![],
        stderr: vec![],
    }]);
    assert!(!cleanup_target_is_confirmed_absent(&locator, SESSION, &live).unwrap());
    assert!(live.seen.borrow()[0].args[1].contains("then exit 1"));

    let absent = PodmanPreflightExecutor::with_outputs([podman_output("")]);
    assert!(cleanup_target_is_confirmed_absent(&locator, SESSION, &absent).unwrap());

    let unreachable = PodmanPreflightExecutor::with_outputs([CommandOutput {
        status: 2,
        stdout: vec![],
        stderr: b"daemon unavailable".to_vec(),
    }]);
    assert!(cleanup_target_is_confirmed_absent(&locator, SESSION, &unreachable).is_err());
}

#[test]
fn setup_smoke_plan_uses_the_configured_local_runtime_and_cleans_up() {
    let plan = setup_smoke_plan(
        &TargetTemplate::LocalPodman(ContainerTemplate {
            image: "ubuntu:24.04".to_owned(),
            pull_policy: ImagePullPolicy::Auto,
            extra_run_args: vec![],
        }),
        "setup-123",
    )
    .unwrap();

    assert_eq!(
        plan.description,
        "smoke test Mjolnir setup target setup-123"
    );
    assert_eq!(plan.commands.len(), 3);
    assert_eq!(plan.commands[0].program, "podman");
    assert!(plan.commands[0].args.contains(&"ubuntu:24.04".to_owned()));
    assert_eq!(plan.commands[1].args.last().unwrap(), "true");
    assert_eq!(plan.commands[2].args[0], "rm");
    assert_eq!(
        plan.commands[2].purpose,
        "remove disposable setup container"
    );
}

#[test]
fn setup_smoke_test_removes_a_container_after_a_failed_exec() {
    let executor = FakeExecutor {
        seen: RefCell::new(vec![]),
        fail_at: Some(1),
    };

    assert!(
        run_setup_smoke_test(
            &TargetTemplate::AppleContainer(ContainerTemplate {
                image: "ubuntu:24.04".to_owned(),
                pull_policy: ImagePullPolicy::Auto,
                extra_run_args: vec![],
            }),
            "setup-123",
            &executor,
        )
        .is_err()
    );
    assert_eq!(executor.seen.borrow().len(), 3);
    assert_eq!(executor.seen.borrow()[2].args[0], "rm");
}

#[test]
fn remote_podman_is_ssh_plus_podman_not_remote_api() {
    let plan = provision_plan(
        &TargetTemplate::SshPodman {
            ssh: ssh(),
            container: ContainerTemplate {
                image: "ghcr.io/example/dev:latest".to_owned(),
                pull_policy: ImagePullPolicy::Auto,
                extra_run_args: vec![],
            },
        },
        SESSION,
        &bundle(),
        &[],
    )
    .unwrap();
    assert!(plan.commands.iter().all(|command| command.program == "ssh"));
    // The default policy launches from the cached image; the daemon's
    // background refresh is what keeps a remote `:latest` tag current.
    assert!(
        plan.commands[0]
            .args
            .last()
            .unwrap()
            .contains("'podman' 'run' '--init'")
    );
    assert!(plan.commands[0].args.last().unwrap().contains(&format!(
        "'--label' '{SESSION_LABEL}={SESSION}' '--label' '{MANAGED_LABEL}=true'"
    )));
    assert!(
        !plan
            .commands
            .iter()
            .flat_map(|command| &command.args)
            .any(|arg| arg.contains("CONTAINER_HOST") || arg == "--remote")
    );
}

#[test]
fn remote_podman_resource_probe_uses_ssh_and_container_cgroups() {
    let locator = TargetLocator::SshPodman {
        ssh: ssh(),
        container_id: resource_name(SESSION).unwrap(),
    };

    let probe = resource_probe(&locator, SESSION).unwrap();

    assert_eq!(probe.memory.program, "ssh");
    assert!(
        probe
            .memory
            .args
            .last()
            .unwrap()
            .contains("memory.swap.current")
    );
    assert_eq!(probe.disk.as_ref().unwrap().program, "ssh");
    assert!(
        probe
            .disk
            .as_ref()
            .unwrap()
            .args
            .last()
            .unwrap()
            .contains("'podman' 'container' 'inspect' '--size'")
    );
}

#[test]
fn ec2_resource_probe_reads_host_pressure_and_session_disk() {
    let locator = TargetLocator::AwsEc2 {
        profile: "default".to_owned(),
        region: "us-east-1".to_owned(),
        instance_id: "i-0123456789abcdef0".to_owned(),
        ssh: ssh(),
        workspace: format!(".local/share/hel/workspaces/{SESSION}"),
    };

    let probe = resource_probe(&locator, SESSION).unwrap();

    assert_eq!(probe.memory.program, "ssh");
    assert!(probe.memory.args.last().unwrap().contains("MemAvailable"));
    assert_eq!(probe.disk.as_ref().unwrap().program, "ssh");
    assert!(
        probe
            .disk
            .as_ref()
            .unwrap()
            .args
            .last()
            .unwrap()
            .contains(&format!(".local/share/hel/workspaces/{SESSION}"))
    );
}

#[test]
fn parses_cgroup_memory_swap_and_writable_disk_usage() {
    let usage = parse_resource_usage(
            b"cpu.percent=37.4\nmemory.current=1073741824\nmemory.max=2147483648\nmemory.swap.current=4096\nmemory.swap.max=max\n",
            Some(b"8192\n"),
        )
        .unwrap();

    assert_eq!(usage.cpu_percent, Some(37));
    assert_eq!(usage.memory_current_bytes, 1_073_741_824);
    assert_eq!(usage.memory_limit_bytes, Some(2_147_483_648));
    assert_eq!(usage.swap_current_bytes, Some(4_096));
    assert_eq!(usage.swap_limit_bytes, None);
    assert_eq!(usage.writable_disk_bytes, Some(8_192));
}

#[test]
fn a_disk_probe_that_answered_garbage_is_a_failure_not_unknown_usage() {
    let memory = b"memory.current=1073741824\n";

    // A probe that never ran leaves the usage unknown.
    let unknown = parse_resource_usage(memory, None).unwrap();
    assert_eq!(unknown.writable_disk_bytes, None);

    // A probe that ran and answered something that is not a byte count
    // measured nothing, and must not be reported as usage.
    let error = parse_resource_usage(memory, Some(b"du: cannot access\n")).unwrap_err();
    assert!(
        error.to_string().contains("instead of a byte count"),
        "{error}"
    );
    assert!(parse_resource_usage(memory, Some(b"")).is_err());
}

#[test]
fn the_ec2_disk_probe_fails_instead_of_undercounting_an_unreadable_path() {
    let directory = tempfile::tempdir().unwrap();
    let measured = directory.path().join("workspace");
    fs::create_dir_all(&measured).unwrap();
    fs::write(measured.join("file"), vec![0_u8; 64 * 1024]).unwrap();
    let missing = directory.path().join("never-created");
    let disk_probe = |paths: &[&Path]| {
        let mut args = vec![
            "-c".to_owned(),
            AWS_SESSION_DISK_USAGE_SCRIPT.to_owned(),
            "sh".to_owned(),
        ];
        args.extend(paths.iter().map(|path| path.to_string_lossy().into_owned()));
        ProcessExecutor
            .execute(&CommandSpec::new("sh", args).purpose("test the EC2 session disk probe"))
            .unwrap()
    };

    let measured_only = disk_probe(&[&measured]);
    assert_eq!(measured_only.status, 0);
    let bytes = parse_disk_usage(&measured_only.stdout).unwrap();
    assert!(bytes >= 64 * 1024, "{bytes}");

    // One unreadable path must fail the probe rather than quietly reporting
    // the total of the paths that did answer.
    let with_missing = disk_probe(&[&measured, &missing]);
    assert_ne!(with_missing.status, 0);
    assert!(
        String::from_utf8_lossy(&with_missing.stderr).contains("never-created"),
        "the failure must name the path: {}",
        String::from_utf8_lossy(&with_missing.stderr)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn host_capacity_counts_zfs_arc_above_its_minimum_as_available_memory() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("meminfo"),
        "MemTotal: 1000 kB\nMemAvailable: 400 kB\nSwapTotal: 10 kB\nSwapFree: 8 kB\n",
    )
    .unwrap();
    let sample = || {
        ProcessExecutor
            .execute(
                &CommandSpec::new(
                    "sh",
                    [
                        "-c".to_owned(),
                        HOST_RESOURCE_USAGE_SCRIPT.to_owned(),
                        "sh".to_owned(),
                        directory.path().to_string_lossy().into_owned(),
                    ],
                )
                .purpose("test host available-memory accounting"),
            )
            .unwrap()
    };

    let without_arc = sample();
    assert_eq!(without_arc.status, 0);
    let values = parse_key_values(&without_arc.stdout);
    assert_eq!(values["memory.current"], "614400");
    assert_eq!(values["memory.max"], "1024000");

    let arcstats = directory.path().join("spl/kstat/zfs");
    fs::create_dir_all(&arcstats).unwrap();
    fs::write(arcstats.join("arcstats"), "c_min 4 102400\nsize 4 409600\n").unwrap();

    let with_arc = sample();
    assert_eq!(with_arc.status, 0);
    let values = parse_key_values(&with_arc.stdout);
    assert_eq!(values["memory.current"], "307200");
    assert_eq!(values["memory.max"], "1024000");
}

#[test]
fn parses_host_and_aws_capacity_outputs() {
    let host = parse_host_capacity(
        b"cpu.percent=62.6\nmemory.current=300\nmemory.max=1000\nlogical.cores=8\n",
    )
    .unwrap();
    assert_eq!(host.cpu_percent, Some(63));
    assert_eq!(host.memory_used_bytes, 300);
    assert_eq!(host.memory_total_bytes, 1_000);
    assert_eq!(host.logical_cores, 8);

    let aws = parse_aws_allocated_capacity(
        b"memory.total=34359738368\nlogical.cores=16\ndisk.total=214748364800\n",
    )
    .unwrap();
    assert_eq!(aws.cpu_percent, None);
    assert_eq!(aws.memory_total_bytes, 34_359_738_368);
    assert_eq!(aws.logical_cores, 16);
    assert_eq!(aws.disk_total_bytes, Some(214_748_364_800));

    assert!(parse_host_capacity(b"cpu.percent=nan\n").is_err());
    assert!(parse_aws_allocated_capacity(b"memory.total=nope\n").is_err());
}

#[test]
fn local_path_completion_returns_directory_components_and_tab_prefix() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("data")).unwrap();
    std::fs::create_dir(directory.path().join("dashboard")).unwrap();
    std::fs::write(directory.path().join("data.txt"), "not a directory").unwrap();
    let prefix = format!("{}/da", directory.path().display());

    let matches = local_directory_completions(&prefix);

    assert_eq!(
        matches,
        vec![
            format!("{}/dashboard/", directory.path().display()),
            format!("{}/data/", directory.path().display()),
        ]
    );
    assert_eq!(path_completion(&prefix, &matches), None);
    assert_eq!(
        path_completion("/srv/da", &["/srv/data/".into(), "/srv/database/".into()],),
        Some("/srv/data".into())
    );
    assert_eq!(
        path_completion("/srv/da", &["/srv/data/".into()]),
        Some("/srv/data/".into())
    );
}

#[test]
fn ssh_directory_check_quotes_the_source_and_distinguishes_missing() {
    let exists = FakeExecutor {
        seen: RefCell::new(vec![]),
        fail_at: None,
    };
    assert!(ssh_directory_exists(&ssh(), Path::new("/srv/user's data"), &exists).unwrap());
    let command = &exists.seen.borrow()[0];
    assert_eq!(command.program, "ssh");
    let remote_command = command.args.last().unwrap();
    assert!(remote_command.starts_with("'test' '-d' "));
    assert!(remote_command.contains("'/srv/user'\\''s data'"));

    let missing = FakeExecutor {
        seen: RefCell::new(vec![]),
        fail_at: Some(0),
    };
    assert!(!ssh_directory_exists(&ssh(), Path::new("/missing"), &missing).unwrap());
}

#[test]
fn bare_project_validation_checks_directory_and_git_repository() {
    let valid = FakeExecutor {
        seen: RefCell::new(vec![]),
        fail_at: None,
    };
    validate_bare_project_directory(&ssh(), Path::new("/srv/project"), &valid).unwrap();
    let seen = valid.seen.borrow();
    assert_eq!(seen.len(), 2);
    assert!(
        seen[0]
            .args
            .last()
            .unwrap()
            .contains("'test' '-d' '/srv/project'")
    );
    assert!(
        seen[1]
            .args
            .last()
            .unwrap()
            .contains("'git' '-C' '/srv/project' 'rev-parse' '--verify' 'HEAD'")
    );
    assert!(seen[0].args.contains(&"ConnectTimeout=3".to_owned()));

    let missing = FakeExecutor {
        seen: RefCell::new(vec![]),
        fail_at: Some(0),
    };
    let error =
        validate_bare_project_directory(&ssh(), Path::new("/missing"), &missing).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("does not exist or is not a directory")
    );
    assert_eq!(missing.seen.borrow().len(), 1);

    let not_git = FakeExecutor {
        seen: RefCell::new(vec![]),
        fail_at: Some(1),
    };
    let error =
        validate_bare_project_directory(&ssh(), Path::new("/srv/plain"), &not_git).unwrap_err();
    assert!(error.to_string().contains("has no valid Git HEAD"));
    assert_eq!(not_git.seen.borrow().len(), 2);
}

#[test]
fn ssh_path_completion_uses_short_timeout_and_fake_executor() {
    let executor = PodmanPreflightExecutor::with_outputs([CommandOutput {
        status: 0,
        stdout: b"/srv/projects/\n/srv/prompts/\n".to_vec(),
        stderr: vec![],
    }]);

    let matches = ssh_directory_completions(&ssh(), "/srv/pr", &executor).unwrap();

    assert_eq!(matches, vec!["/srv/projects/", "/srv/prompts/"]);
    let command = &executor.seen.borrow()[0];
    assert_eq!(command.program, "ssh");
    assert!(command.args.contains(&"ConnectTimeout=3".to_owned()));
    assert!(
        command
            .args
            .last()
            .unwrap()
            .contains("ls -d -- '/srv/pr'*/")
    );
}

/// Provisioning has to know exactly when its target came into existence,
/// because every step after that owes the target's teardown on failure.
#[test]
fn every_provisioning_plan_names_the_command_that_creates_its_target() {
    let container = ContainerTemplate {
        image: "ubuntu:24.04".to_owned(),
        pull_policy: ImagePullPolicy::Auto,
        extra_run_args: vec![],
    };
    let creating = [
        (
            TargetTemplate::LocalPodman(container.clone()),
            "start session container",
        ),
        (
            TargetTemplate::LocalDocker(container.clone()),
            "start session container",
        ),
        (
            TargetTemplate::AppleContainer(container.clone()),
            "start session container",
        ),
        (
            TargetTemplate::SshPodman {
                ssh: ssh(),
                container,
            },
            "start remote Podman container",
        ),
        (
            TargetTemplate::SshBare {
                ssh: ssh(),
                workspace_prefix: "/srv/hel".to_owned(),
            },
            "create SSH session workspace",
        ),
        (
            TargetTemplate::AwsEc2(AwsTemplate {
                profile: "work".to_owned(),
                region: "us-east-2".to_owned(),
                launch_template: "hel-dev".to_owned(),
                launch_template_version: None,
                instance_type: None,
                ssh: ssh(),
            }),
            "launch EC2 session instance",
        ),
    ];
    for (template, purpose) in creating {
        let plan = provision_plan(&template, SESSION, &bundle(), &[]).unwrap();
        assert_eq!(
            plan.description,
            format!("provision Mjolnir session {SESSION}")
        );
        let (creation, remainder) = plan.split_at_target_creation().unwrap();

        assert_eq!(creation.commands.last().unwrap().purpose, purpose);
        assert_eq!(
            creation
                .commands
                .iter()
                .filter(|command| command.creates_target)
                .count(),
            1
        );
        assert!(
            !remainder
                .commands
                .iter()
                .any(|command| command.creates_target)
        );
        assert_eq!(
            creation.commands.len() + remainder.commands.len(),
            plan.commands.len()
        );
        // Cloning a bundle happens against a target that already exists.
        assert_eq!(
            remainder
                .commands
                .iter()
                .any(|command| command.purpose.starts_with("clone ")),
            !matches!(template, TargetTemplate::AwsEc2(_)),
        );
    }

    // An existing project directory is never created, so nothing about it
    // can leak.
    assert!(
        provision_bare_project_plan(&TargetTemplate::LocalBare, SESSION, "/srv/project")
            .unwrap()
            .split_at_target_creation()
            .is_none()
    );
}

#[test]
fn aws_plan_tags_instance_and_close_uses_recorded_id() {
    let template = TargetTemplate::AwsEc2(AwsTemplate {
        profile: "work".to_owned(),
        region: "us-east-2".to_owned(),
        launch_template: "hel-dev".to_owned(),
        launch_template_version: Some("3".to_owned()),
        instance_type: Some("m8i-flex.2xlarge".into()),
        ssh: ssh(),
    });
    let provision = provision_plan(&template, SESSION, &bundle(), &[]).unwrap();
    assert_eq!(provision.commands.len(), 1);
    assert!(
        provision.commands[0].args.windows(2).any(|args| args
            == managed_resource_identity_args(ManagedResourceKind::Ec2Instance, SESSION))
    );
    assert!(
        provision.commands[0]
            .args
            .windows(2)
            .any(|args| { args == ["--instance-type", "m8i-flex.2xlarge"] })
    );
    let close = close_plan(
        &TargetLocator::AwsEc2 {
            profile: "work".to_owned(),
            region: "us-east-2".to_owned(),
            instance_id: "i-0123456789abcdef0".to_owned(),
            ssh: ssh(),
            workspace: format!(".local/share/hel/workspaces/{SESSION}"),
        },
        SESSION,
    )
    .unwrap();
    assert_eq!(
        close.commands[0].args.last().unwrap(),
        "i-0123456789abcdef0"
    );
}

#[test]
fn tilde_workspace_prefix_becomes_home_relative() {
    // Remote commands are single-quoted, so a literal "~" would name a
    // real directory instead of the login home.
    let template = TargetTemplate::SshBare {
        ssh: ssh(),
        workspace_prefix: "~/hel".into(),
    };
    assert_eq!(
        workspace_for(&template, SESSION).unwrap(),
        format!("hel/{SESSION}")
    );

    for degenerate in ["~", "~/"] {
        let template = TargetTemplate::SshBare {
            ssh: ssh(),
            workspace_prefix: degenerate.into(),
        };
        assert!(
            workspace_for(&template, SESSION).is_err(),
            "prefix {degenerate:?} must be rejected"
        );
    }
}

#[test]
fn shell_arguments_are_single_quoted_at_ssh_boundary() {
    let hostile = "repo'; touch /tmp/pwned; echo '";
    let command = ssh_command(&ssh(), ["git", "clone", "--", hostile]);
    assert_eq!(
        command.args.last().unwrap(),
        "'git' 'clone' '--' 'repo'\\''; touch /tmp/pwned; echo '\\'''"
    );
}

#[test]
fn bare_project_plan_leaves_project_validation_to_dialog_and_launch() {
    let local =
        provision_bare_project_plan(&TargetTemplate::LocalBare, SESSION, "/home/me/project")
            .unwrap();
    assert_eq!(
        local.description,
        format!("provision Mjolnir session {SESSION}")
    );
    assert!(local.commands.is_empty());

    let template = TargetTemplate::SshBare {
        ssh: ssh(),
        workspace_prefix: ".local/share/hel/workspaces".into(),
    };
    let provision = provision_bare_project_plan(&template, SESSION, "/srv/project").unwrap();
    let commands = provision
        .commands
        .iter()
        .map(|command| command.args.last().unwrap().as_str())
        .collect::<Vec<_>>();
    assert!(commands.is_empty());

    let locator = TargetLocator::SshBare {
        ssh: ssh(),
        workspace: format!(".local/share/hel/workspaces/{SESSION}"),
    };
    let close = close_plan(&locator, SESSION).unwrap();
    assert!(
        !close.commands[0]
            .args
            .last()
            .unwrap()
            .contains("/srv/project")
    );
}

#[test]
fn local_bare_worker_commands_are_direct_and_cleanup_is_exact() {
    let worker_root = format!("/var/lib/hel/workers/{SESSION}");
    let locator = TargetLocator::LocalBare {
        worker_root: worker_root.clone(),
    };

    let reconnect = reconnect_plan(&locator, SESSION).unwrap();
    assert_eq!(
        reconnect.description,
        format!("reconnect Mjolnir session {SESSION}")
    );
    assert_eq!(reconnect.commands[0].purpose, "connect to Mjolnir worker");
    assert_eq!(reconnect.commands[0].program, format!("{worker_root}/hel"));
    assert_eq!(
        reconnect.commands[0].args,
        ["worker", "proxy", "--root", worker_root.as_str()]
    );
    let close = close_plan(&locator, SESSION).unwrap();
    assert_eq!(
        close.description,
        format!("close Mjolnir session {SESSION}")
    );
    assert_eq!(
        close.commands[0].purpose,
        "stop the local Mjolnir worker and remove exact local Mjolnir worker state"
    );
    assert_eq!(close.commands[0].program, "sh");
    assert_eq!(close.commands[0].args[0], "-c");
    let script = &close.commands[0].args[1];
    assert!(script.contains(&format!("hel_root='{worker_root}'")));
    assert!(script.ends_with(&format!("rm -rf -- '{worker_root}'\n")));
}

/// A leaked daemon that survives teardown recreates the root it is asked
/// to forget, so the kill has to be part of the same cleanup command.
#[test]
fn bare_cleanup_stops_the_recorded_worker_before_removing_its_root() {
    let worker_root = format!("/var/lib/hel/workers/{SESSION}");
    let local = close_plan(
        &TargetLocator::LocalBare {
            worker_root: worker_root.clone(),
        },
        SESSION,
    )
    .unwrap();
    let remote = close_plan(
        &TargetLocator::SshBare {
            ssh: ssh(),
            workspace: format!(".local/share/hel/workspaces/{SESSION}"),
        },
        SESSION,
    )
    .unwrap();

    assert_eq!(
        remote.commands[0].purpose,
        "stop the remote Mjolnir worker and remove exact SSH session workspace and runtime state"
    );

    for script in [
        local.commands[0].args[1].clone(),
        remote.commands[0].args.last().unwrap().clone(),
    ] {
        let kill = script
            .find("hel_signal TERM")
            .expect("cleanup signals the worker");
        let remove = script.find("rm -rf").expect("cleanup removes the root");
        assert!(kill < remove, "the worker must die before its root does");
        // The pidfile is the identity check; a reused PID running
        // something else must survive.
        assert!(script.contains("worker.pid"));
        assert!(script.contains(r#"hel_match="hel worker run --root $hel_root""#));
        assert!(script.contains(r#"hel_match_home="hel worker run --root $HOME/$hel_root""#));
        assert!(script.contains("hel_is_worker"));
        assert!(script.contains("hel_signal KILL"));
        assert!(script.contains("worker still running after stop"));
        assert!(
            !script.contains("grep -F"),
            "leftover detection must not grep the match string; grep's own argv contains it"
        );
        assert!(!script.contains("pkill"));
    }
    assert!(
        remote.commands[0]
            .args
            .last()
            .unwrap()
            .contains(&format!(".local/share/hel/workers/{SESSION}")),
    );
}

#[cfg(unix)]
#[test]
fn stop_worker_script_succeeds_when_no_daemon_is_running() {
    let worker_root = format!("/tmp/hel-stop-absent-{}-{SESSION}", std::process::id());
    let script = stop_worker_daemon_script(&worker_root);
    let output = std::process::Command::new("sh")
        .args(["-c", &script])
        .output()
        .expect("run stop script");
    assert!(
        output.status.success(),
        "stop with no daemon must not false-positive leftover detection: status={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn stop_worker_script_kills_a_matching_daemon_and_is_idempotent() {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::ExitStatusExt;

    let directory = tempfile::Builder::new()
        .prefix("hel-stop-")
        .tempdir_in("/tmp")
        .unwrap();
    let worker_root = directory.path().canonicalize().unwrap().join("worker");
    std::fs::create_dir_all(&worker_root).unwrap();
    let fake_hel = worker_root.join("hel");
    std::fs::write(&fake_hel, "#!/bin/sh\nwhile true; do sleep 1; done\n").unwrap();
    std::fs::set_permissions(&fake_hel, std::fs::Permissions::from_mode(0o700)).unwrap();
    let root = worker_root.to_str().unwrap();
    let mut child = std::process::Command::new(&fake_hel)
        .args(["worker", "run", "--root", root, "--config", "launch.json"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("start fake worker");
    std::fs::write(worker_root.join("worker.pid"), format!("{}\n", child.id())).unwrap();

    let liveness = std::process::Command::new("sh")
        .args(["-c", &worker_daemon_liveness_script(root)])
        .output()
        .expect("probe starting fake worker");
    assert!(liveness.status.success());
    assert_eq!(liveness.stdout, b"starting\n");

    let _relay = std::os::unix::net::UnixListener::bind(worker_root.join("control.sock"))
        .expect("publish fake worker relay socket");
    let liveness = std::process::Command::new("sh")
        .args(["-c", &worker_daemon_liveness_script(root)])
        .output()
        .expect("probe live fake worker");
    assert!(liveness.status.success());
    assert_eq!(liveness.stdout, b"alive\n");

    let script = stop_worker_daemon_script(root);
    let output = std::process::Command::new("sh")
        .args(["-c", &script])
        .output()
        .expect("run stop script");
    assert!(
        output.status.success(),
        "stop must kill the matching daemon: status={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let status = child.wait().expect("reap fake worker");
    assert!(
        !status.success() || status.signal().is_some(),
        "fake worker should have been signaled, got {status:?}"
    );

    let output = std::process::Command::new("sh")
        .args(["-c", &script])
        .output()
        .expect("run stop script again");
    assert!(
        output.status.success(),
        "second stop must be a no-op: status={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let liveness = std::process::Command::new("sh")
        .args(["-c", &worker_daemon_liveness_script(root)])
        .output()
        .expect("probe stopped fake worker");
    assert!(liveness.status.success());
    assert_eq!(liveness.stdout, b"dead\n");
}

/// Resume reuses a bare target's worker root, so anything left writing
/// there and any stale relay state has to go before the restore seeds it.
#[test]
fn resume_cleanup_clears_relay_state_only_for_reused_bare_roots() {
    let local = clear_relay_state_plan(
        &TargetLocator::LocalBare {
            worker_root: format!("/var/lib/hel/workers/{SESSION}"),
        },
        SESSION,
    )
    .unwrap()
    .expect("raw localhost reuses its worker root");
    assert_eq!(
        local.purpose,
        "stop a leaked local Mjolnir worker and clear its relay state"
    );
    let script = &local.args[1];
    assert!(script.contains("hel_signal TERM"));
    assert!(script.contains(&format!(
        "rm -rf -- '/var/lib/hel/workers/{SESSION}/relay-state.json' \
             '/var/lib/hel/workers/{SESSION}/relay-journal'"
    )));

    let remote = clear_relay_state_plan(
        &TargetLocator::SshBare {
            ssh: ssh(),
            workspace: format!(".local/share/hel/workspaces/{SESSION}"),
        },
        SESSION,
    )
    .unwrap()
    .expect("an SSH host reuses its worker root");
    assert_eq!(
        remote.purpose,
        "stop a leaked remote Mjolnir worker and clear its relay state"
    );
    assert!(remote.args.last().unwrap().contains(&format!(
        ".local/share/hel/workers/{SESSION}/relay-state.json"
    )));

    // Containers and instances are rebuilt from nothing on resume.
    assert!(
        clear_relay_state_plan(
            &TargetLocator::LocalPodman {
                container_id: resource_name(SESSION).unwrap(),
            },
            SESSION,
        )
        .unwrap()
        .is_none()
    );
}

#[test]
fn podman_cleanup_ignores_an_already_absent_container() {
    let name = resource_name(SESSION).unwrap();
    let local = close_plan(
        &TargetLocator::LocalPodman {
            container_id: name.clone(),
        },
        SESSION,
    )
    .unwrap();
    assert_eq!(local.commands[0].program, "sh");
    let local_script = &local.commands[0].args[1];
    let remove_container = local_script.find("podman rm --force --ignore").unwrap();
    let remove_cache = local_script.find(".cache/mjolnir/git/sessions").unwrap();
    assert!(remove_container < remove_cache);
    assert_eq!(local.commands[0].args.last().unwrap(), SESSION);

    let remote = close_plan(
        &TargetLocator::SshPodman {
            ssh: ssh(),
            container_id: name,
        },
        SESSION,
    )
    .unwrap();
    let remote = remote.commands[0].args.last().unwrap();
    let remove_container = remote.find("podman rm --force --ignore").unwrap();
    let remove_cache = remote.find(".cache/mjolnir/git/sessions").unwrap();
    assert!(remove_container < remove_cache);
    assert!(remote.contains(SESSION));
}

#[test]
fn docker_cleanup_removes_container_then_volumes_then_overlay_backing_files() {
    let name = resource_name(SESSION).unwrap();
    let close = close_plan(&TargetLocator::LocalDocker { container_id: name }, SESSION).unwrap();
    let command = &close.commands[0];
    assert_eq!(command.program, "sh");
    let script = &command.args[1];
    let remove_container = script.find("docker rm --force").unwrap();
    let list_volumes = script.find("docker volume ls").unwrap();
    let remove_volumes = script.find("docker volume rm --force").unwrap();
    let remove_overlay = script.find(".cache/mjolnir/docker-overlays").unwrap();
    assert!(remove_container < list_volumes);
    assert!(list_volumes < remove_volumes);
    assert!(remove_volumes < remove_overlay);
    assert!(script.contains("if [ \"$status\" -eq 0 ]"));
    assert!(script.contains("label=dev.mj.managed=true"));
    assert!(script.contains("label=dev.mj.session=$2"));
    assert!(script.contains("true|$2"));
    assert!(script.contains("refusing to remove a Docker container Mjolnir does not own"));
}

#[test]
fn close_rejects_broad_or_mismatched_targets() {
    let broad = TargetLocator::SshBare {
        ssh: ssh(),
        workspace: ".local/share/hel/workspaces".to_owned(),
    };
    assert!(close_plan(&broad, SESSION).is_err());
    let mismatch = TargetLocator::LocalPodman {
        container_id: "hel-someone-abcdef".to_owned(),
    };
    assert!(close_plan(&mismatch, SESSION).is_err());
    let root = TargetLocator::SshBare {
        ssh: ssh(),
        workspace: "/".to_owned(),
    };
    assert!(close_plan(&root, SESSION).is_err());
}

#[test]
fn bundle_rejects_traversal_and_duplicate_destinations() {
    let mut invalid = bundle();
    invalid.repositories[0].destination = "../escape".to_owned();
    assert!(invalid.validate().is_err());
    let mut duplicate = bundle();
    duplicate.repositories[1].destination = "app".to_owned();
    assert!(duplicate.validate().is_err());
}

struct FakeExecutor {
    seen: RefCell<Vec<CommandSpec>>,
    fail_at: Option<usize>,
}

impl CommandExecutor for FakeExecutor {
    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        let index = self.seen.borrow().len();
        self.seen.borrow_mut().push(command.clone());
        Ok(CommandOutput {
            status: i32::from(self.fail_at == Some(index)),
            stdout: vec![],
            stderr: b"failure".to_vec(),
        })
    }
}

#[test]
fn executor_stops_at_first_failed_command() {
    let executor = FakeExecutor {
        seen: RefCell::new(vec![]),
        fail_at: Some(1),
    };
    let plan = CommandPlan {
        description: "test".to_owned(),
        commands: vec![
            CommandSpec::new("one", std::iter::empty::<String>()).purpose("one"),
            CommandSpec::new("two", std::iter::empty::<String>()).purpose("two"),
            CommandSpec::new("three", std::iter::empty::<String>()).purpose("three"),
        ],
    };
    assert!(plan.execute(&executor).is_err());
    assert_eq!(executor.seen.borrow().len(), 2);
}

/// A `Sync` counterpart to [`FakeExecutor`], usable with
/// [`CommandPlan::execute_concurrent`].
struct SyncFakeExecutor {
    seen: Mutex<Vec<CommandSpec>>,
    fail_at: Option<usize>,
}

impl CommandExecutor for SyncFakeExecutor {
    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        let mut seen = self.seen.lock().unwrap();
        let index = seen.len();
        seen.push(command.clone());
        Ok(CommandOutput {
            status: i32::from(self.fail_at == Some(index)),
            stdout: vec![],
            stderr: b"failure".to_vec(),
        })
    }
}

#[test]
fn ungrouped_plans_behave_identically_under_both_execution_methods() {
    let commands = vec![
        CommandSpec::new("one", std::iter::empty::<String>()).purpose("one"),
        CommandSpec::new("two", std::iter::empty::<String>()).purpose("two"),
        CommandSpec::new("three", std::iter::empty::<String>()).purpose("three"),
    ];
    let sequential_plan = CommandPlan {
        description: "test".to_owned(),
        commands: commands.clone(),
    };
    let concurrent_plan = CommandPlan {
        description: "test".to_owned(),
        commands,
    };

    let sequential_executor = SyncFakeExecutor {
        seen: Mutex::new(vec![]),
        fail_at: None,
    };
    let sequential_outputs = sequential_plan.execute(&sequential_executor).unwrap();

    let concurrent_executor = SyncFakeExecutor {
        seen: Mutex::new(vec![]),
        fail_at: None,
    };
    let concurrent_outputs = concurrent_plan
        .execute_concurrent(&concurrent_executor)
        .unwrap();

    assert_eq!(sequential_outputs, concurrent_outputs);
    assert_eq!(
        sequential_executor.seen.into_inner().unwrap(),
        concurrent_executor.seen.into_inner().unwrap(),
        "an ungrouped plan runs its commands in the same order either way"
    );
}

/// Blocks every command on a barrier sized to the batch, so this only
/// returns if [`CommandPlan::execute_concurrent`] actually starts the
/// whole batch before any command in it completes.
struct BarrierExecutor {
    seen: Mutex<Vec<CommandSpec>>,
    barrier: Barrier,
}

impl CommandExecutor for BarrierExecutor {
    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        self.seen.lock().unwrap().push(command.clone());
        self.barrier.wait();
        Ok(CommandOutput {
            status: 0,
            stdout: vec![],
            stderr: vec![],
        })
    }
}

#[test]
fn grouped_commands_run_concurrently() {
    let plan = CommandPlan {
        description: "test".to_owned(),
        commands: vec![
            CommandSpec::new("one", std::iter::empty::<String>())
                .purpose("one")
                .parallel_group(7),
            CommandSpec::new("two", std::iter::empty::<String>())
                .purpose("two")
                .parallel_group(7),
            CommandSpec::new("three", std::iter::empty::<String>())
                .purpose("three")
                .parallel_group(7),
        ],
    };
    let executor = BarrierExecutor {
        seen: Mutex::new(vec![]),
        barrier: Barrier::new(3),
    };

    let outputs = plan.execute_concurrent(&executor).unwrap();

    assert_eq!(outputs.len(), 3);
    assert_eq!(executor.seen.into_inner().unwrap().len(), 3);
}

/// Fails "first" slowly and "third" immediately, so a plan-order failure
/// report (rather than a completion-order one) can only pick "first".
struct OrderSensitiveFailureExecutor {
    seen: Mutex<Vec<CommandSpec>>,
}

impl CommandExecutor for OrderSensitiveFailureExecutor {
    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        self.seen.lock().unwrap().push(command.clone());
        match command.purpose.as_str() {
            "first" => {
                std::thread::sleep(Duration::from_millis(50));
                Ok(CommandOutput {
                    status: 1,
                    stdout: vec![],
                    stderr: b"first failed".to_vec(),
                })
            }
            "third" => Ok(CommandOutput {
                status: 1,
                stdout: vec![],
                stderr: b"third failed".to_vec(),
            }),
            _ => Ok(CommandOutput {
                status: 0,
                stdout: vec![],
                stderr: vec![],
            }),
        }
    }
}

#[test]
fn batch_failure_reports_the_first_in_plan_order_failure_and_blocks_later_commands() {
    let plan = CommandPlan {
        description: "test".to_owned(),
        commands: vec![
            CommandSpec::new("a", std::iter::empty::<String>())
                .purpose("first")
                .parallel_group(3),
            CommandSpec::new("b", std::iter::empty::<String>())
                .purpose("second")
                .parallel_group(3),
            CommandSpec::new("c", std::iter::empty::<String>())
                .purpose("third")
                .parallel_group(3),
            CommandSpec::new("d", std::iter::empty::<String>()).purpose("fourth"),
        ],
    };
    let executor = OrderSensitiveFailureExecutor {
        seen: Mutex::new(vec![]),
    };

    let error = plan.execute_concurrent(&executor).unwrap_err();

    assert!(
        error.to_string().starts_with("first failed with status 1"),
        "expected the plan-order failure (\"first\"), got: {error}"
    );
    let seen = executor.seen.into_inner().unwrap();
    assert_eq!(
        seen.len(),
        3,
        "the whole failing batch starts even though it fails"
    );
    assert!(
        !seen.iter().any(|command| command.purpose == "fourth"),
        "a command after a failed batch must not start"
    );
}

#[test]
fn failure_before_a_batch_prevents_the_batch_from_starting() {
    let plan = CommandPlan {
        description: "test".to_owned(),
        commands: vec![
            CommandSpec::new("gate", std::iter::empty::<String>()).purpose("gate"),
            CommandSpec::new("a", std::iter::empty::<String>())
                .purpose("batch-a")
                .parallel_group(9),
            CommandSpec::new("b", std::iter::empty::<String>())
                .purpose("batch-b")
                .parallel_group(9),
        ],
    };
    let executor = SyncFakeExecutor {
        seen: Mutex::new(vec![]),
        fail_at: Some(0),
    };

    assert!(plan.execute_concurrent(&executor).is_err());
    assert_eq!(
        executor.seen.into_inner().unwrap().len(),
        1,
        "a batch must not start once an earlier command has already failed"
    );
}

#[test]
fn cancellable_executor_terminates_a_running_process() {
    let cancelled = Arc::new(AtomicBool::new(false));
    let executor = CancellableProcessExecutor::new(cancelled.clone());
    let cancel = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        cancelled.store(true, Ordering::Release);
    });
    let started = std::time::Instant::now();
    let error = executor
        .execute(&CommandSpec::new("sh", ["-c", "sleep 30"]).purpose("test cancellable process"))
        .unwrap_err();
    cancel.join().unwrap();
    assert!(error.to_string().contains("operation cancelled"));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn cancellable_executor_enforces_its_inline_deadline() {
    let executor = CancellableProcessExecutor::with_timeout(Duration::from_millis(50));
    let started = std::time::Instant::now();

    let error = executor
        .execute(
            &CommandSpec::new("sh", ["-c", "sleep 30"]).purpose("test bounded process execution"),
        )
        .unwrap_err();

    assert!(error.to_string().contains("operation cancelled"));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn bounded_executor_times_out_naming_the_probe_and_still_runs_the_next_one() {
    let executor = BoundedProcessExecutor::new(Duration::from_secs(1));
    let started = std::time::Instant::now();

    let error = executor
        .execute(&CommandSpec::new("sh", ["-c", "sleep 30"]).purpose("check a wedged prerequisite"))
        .unwrap_err();

    assert!(started.elapsed() < Duration::from_secs(5));
    let message = error.to_string();
    assert!(message.contains("`sh`"), "{message}");
    assert!(
        message.contains("check a wedged prerequisite"),
        "the timeout must name the probe that hung: {message}"
    );

    // Each command gets its own deadline, so one hung probe does not
    // cancel every probe that follows it.
    let output = executor
        .execute(&CommandSpec::new("sh", ["-c", "printf ready"]).purpose("check the next one"))
        .unwrap();
    assert_eq!(output.status, 0);
    assert_eq!(output.stdout, b"ready");
}

#[test]
fn cancellable_executor_interrupts_a_blocked_stdin_pipe() {
    let cancelled = Arc::new(AtomicBool::new(false));
    let executor = CancellableProcessExecutor::new(cancelled.clone());
    let cancel = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        cancelled.store(true, Ordering::Release);
    });
    let mut input = std::io::Cursor::new(vec![0_u8; 16 * 1024 * 1024]);
    let started = std::time::Instant::now();

    let error = executor
        .execute_with_stdin(
            &CommandSpec::new("sh", ["-c", "sleep 30"])
                .purpose("test cancellation while streaming"),
            &mut input,
        )
        .unwrap_err();

    cancel.join().unwrap();
    assert!(error.to_string().contains("operation cancelled"));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn bootstrap_probes_at_remote_container_boundary() {
    let plan = bootstrap_probe_plan(
        ExecutionBoundary::SshPodman {
            ssh: &ssh(),
            container_id: "abcdef012345",
        },
        HarnessProbe {
            executable: "codex",
            version_args: &["--version"],
            bridge_executable: Some("codex-acp"),
        },
    )
    .unwrap();
    assert_eq!(plan.commands.len(), 3);
    assert!(
        plan.commands[0]
            .args
            .last()
            .unwrap()
            .contains("'podman' 'exec' '-i' 'abcdef012345' 'codex' '--version'")
    );
}
