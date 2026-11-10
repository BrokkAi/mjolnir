use super::*;
use crate::controller::test_support::{
    IsolatedTest, checkpoint_test_session, write_network_checkpoint_archive,
};
use mj_core::move_workspace::{WorkspaceAssessment, WorkspaceTransfer, WorkspaceTransferPhase};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

const ID: &str = "0123456789abcdef0123456789abcdef";
const INSTANCE: &str = "i-1234567890abcdef0";

fn isolated(name: &str) -> bool {
    if std::env::var_os("MJ_EC2_DESTINATION_TEST").is_some() {
        return true;
    }
    let root = tempfile::tempdir().unwrap();
    IsolatedTest::new(crate::controller::test_support::test_name(
        module_path!(),
        name,
    ))
    .env("MJ_EC2_DESTINATION_TEST", "1")
    .env("MJ_INSTANCE", "move-ec2")
    .isolated_store(root.path())
    .run();
    false
}

pub(in crate::controller::move_session) fn ec2_target() -> TargetTemplate {
    TargetTemplate::AwsEc2 {
        aws_profile: Some("test".into()),
        region: "us-east-1".into(),
        launch_template: "lt-0123456789abcdef0".into(),
        launch_template_version: None,
        ssh_user: "ubuntu".into(),
        address_source: mj_core::config::AwsAddressSource::PublicIp,
        identity_file: None,
        ssh_args: Vec::new(),
    }
}

fn fixture() -> (Controller, MoveOperation) {
    let mut session = checkpoint_test_session(ID);
    session.target = Some(TargetLocator::LocalPodman {
        container_id: format!("mj-{ID}"),
        workspace_storage: Default::default(),
        borrowed_from: None,
    });
    let mut config = mj_core::config::Config::default();
    config.targets.insert("ec2".into(), ec2_target());
    // The repository `write_network_checkpoint_archive` records.
    config.bundles.insert(
        session.bundle_id.clone(),
        mj_core::config::ProjectBundle {
            primary_repo: "project".into(),
            repositories: vec![mj_core::config::ProjectRepository {
                id: "project".into(),
                github: Some("https://fetch.example.test/project.git".into()),
                local: None,
                destination: "project".into(),
                git_ref: None,
            }],
        },
    );
    let state = mj_core::state::State {
        sessions: [(ID.into(), session.clone())].into_iter().collect(),
        ..Default::default()
    };
    crate::database::save_state(&state).unwrap();
    let mut operation = super::super::tests::source_recovery_operation(&session);
    operation.phase = MovePhase::Preparing;
    operation.selection.target_template_id = Some("ec2".into());
    operation.workspace_transfer = Some(WorkspaceTransfer {
        assessment: WorkspaceAssessment {
            required_bytes: 200 * 1024,
            ..Default::default()
        },
        source: Box::new(session),
        source_stage: "/source-stage".into(),
        controller_stage: "/controller-stage".into(),
        phase: WorkspaceTransferPhase::Planned,
    });
    let mut destination = pending_launch_recorded_by_an_older_daemon("stored-client-token");
    destination.state = PreparedDestinationState::Created {
        instance_id: INSTANCE.into(),
    };
    operation.prepared_destination = Some(destination);
    crate::database::save_move_operation(&operation).unwrap();
    (Controller { config, state }, operation)
}

#[derive(Default)]
struct Ec2 {
    commands: Mutex<Vec<CommandSpec>>,
    fail_boot: AtomicBool,
    fail_cleanup: AtomicBool,
    /// The instance allows IMDSv1 until `modify-instance-metadata-options`
    /// succeeds.
    imds_optional: AtomicBool,
    /// `modify-instance-metadata-options` fails.
    fail_imds_enforcement: AtomicBool,
    low_disk: AtomicBool,
    handoff: AtomicBool,
    resumable: AtomicBool,
}

impl CommandExecutor for Ec2 {
    fn begin_resumable_work(&self) -> Result<()> {
        self.resumable.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn end_resumable_work(&self, _label: &'static str) -> Result<()> {
        self.resumable.store(false, Ordering::SeqCst);
        ensure!(
            !self.handoff.load(Ordering::SeqCst),
            "daemon handoff interrupted preparation"
        );
        Ok(())
    }

    fn execute(&self, command: &CommandSpec) -> Result<targets::CommandOutput> {
        assert!(!command.args.iter().any(|arg| arg == "run-instances"));
        self.commands.lock().unwrap().push(command.clone());
        let mut stdout = Vec::new();
        match command.purpose.as_str() {
            "wait for EC2 session instance to run" => {
                assert!(
                    self.resumable.load(Ordering::SeqCst),
                    "boot must release upgrade admission"
                );
                let saved = crate::database::load_move_operation(ID)?.unwrap();
                assert_eq!(
                    saved.prepared_destination.unwrap().instance_id(),
                    Some(INSTANCE)
                );
                assert_eq!(
                    crate::database::load_session_state(ID)?,
                    Some(SessionState::Running)
                );
                if self.fail_boot.load(Ordering::SeqCst) {
                    bail!("boot failed");
                }
            }
            "check EC2 instance metadata options" => {
                // The shape `describe-instances --query ...MetadataOptions`
                // printed in the recorded run (aws-run-instances-imdsv2.json).
                let mut options: serde_json::Value = serde_json::from_str(include_str!(
                    "../../../../tests/fixtures/aws-run-instances-imdsv2.json"
                ))
                .unwrap();
                let mut options = options["Instances"][0]["MetadataOptions"].take();
                // The recording was taken right after launch, while pending.
                options["State"] = "applied".into();
                if self.imds_optional.load(Ordering::SeqCst) {
                    options["HttpTokens"] = "optional".into();
                }
                stdout = serde_json::to_vec(&options).unwrap();
            }
            "require IMDSv2 on EC2 instance" => {
                assert_eq!(argument(&command.args, "--instance-id")?, INSTANCE);
                assert_eq!(argument(&command.args, "--http-tokens")?, "required");
                if self.fail_imds_enforcement.load(Ordering::SeqCst) {
                    return Ok(targets::CommandOutput {
                        status: 254,
                        stdout: Vec::new(),
                        stderr: b"UnauthorizedOperation".to_vec(),
                    });
                }
                self.imds_optional.store(false, Ordering::SeqCst);
                // Expected shape from the AWS CLI reference; not live-verified.
                stdout = include_bytes!(
                    "../../../../tests/fixtures/aws-modify-instance-metadata-options.json"
                )
                .to_vec();
            }
            "resolve EC2 session address" => stdout = b"127.0.0.1\n".to_vec(),
            "check prepared EC2 Move staging and workspace space" => {
                let free = if self.low_disk.load(Ordering::SeqCst) {
                    1
                } else {
                    10_000_000
                };
                stdout = format!("Filesystem 1024-blocks Used Available Capacity Mounted\n/dev/ec2 20000000 100 {free} 1% /\n").into_bytes();
            }
            "reconcile EC2 Move launch acknowledgement" => {
                stdout = format!(
                    r#"{{"Reservations":[{{"Instances":[{{"InstanceId":"{INSTANCE}"}}]}}]}}"#
                )
                .into_bytes();
            }
            "terminate exact EC2 session instance" => {
                assert_eq!(argument(&command.args, "--instance-ids")?, INSTANCE);
                assert!(matches!(
                    crate::database::load_move_operation(ID)?
                        .unwrap()
                        .prepared_destination
                        .unwrap()
                        .state,
                    PreparedDestinationState::CleanupPending { .. }
                ));
                if self.fail_cleanup.load(Ordering::SeqCst) {
                    bail!("termination unavailable");
                }
            }
            "confirm EC2 Move destination termination" => {}
            "wait for EC2 SSH availability"
            | "create prepared EC2 workspace"
            | "install Git"
            | "install supported rsync"
            | "check prepared EC2 Move transport" => {}
            other => panic!("unexpected fake EC2 command {other}: {command:?}"),
        }
        Ok(targets::CommandOutput {
            status: 0,
            stdout,
            stderr: Vec::new(),
        })
    }
}

#[test]
fn failed_ec2_boot_cleans_up_without_stopping_the_source() {
    if !isolated("failed_ec2_boot_cleans_up_without_stopping_the_source") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let (mut controller, mut operation) = fixture();
    let cloud = Ec2 {
        fail_boot: AtomicBool::new(true),
        ..Default::default()
    };
    let error = controller
        .recover_prepared_ec2_destination(&mut operation, &cloud)
        .unwrap_err();
    let result = controller
        .finish_move_result(&mut operation, Err(error), &cloud)
        .unwrap();
    assert_eq!(result.outcome, "failed");
    assert!(
        result
            .recovery
            .unwrap()
            .contains("Source retained and still running")
    );
    let session = crate::database::load_session_record(ID).unwrap().unwrap();
    assert_eq!(session.state, SessionState::Running);
    assert_eq!(session.target, operation.source_target);
    assert!(matches!(
        operation.prepared_destination.unwrap().state,
        PreparedDestinationState::Released
    ));
}

#[test]
fn cancellation_after_a_lost_launch_acknowledgement_only_discovers_and_terminates() {
    if !isolated("cancellation_after_a_lost_launch_acknowledgement_only_discovers_and_terminates") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let (controller, mut operation) = fixture();
    operation.prepared_destination =
        Some(pending_launch_recorded_by_an_older_daemon("uncertain-ack"));
    crate::database::save_move_operation(&operation).unwrap();
    let cloud = Ec2::default();
    controller
        .cleanup_prepared_move_destination(&mut operation, &cloud)
        .unwrap();
    assert_eq!(
        cloud
            .commands
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.purpose == "launch EC2 Move destination")
            .count(),
        0
    );
    assert!(matches!(
        operation.prepared_destination.unwrap().state,
        PreparedDestinationState::Released
    ));
    assert_eq!(
        crate::database::load_session_state(ID).unwrap(),
        Some(SessionState::Running)
    );
}

#[test]
fn failed_ec2_cleanup_remains_durable_and_can_be_retried() {
    if !isolated("failed_ec2_cleanup_remains_durable_and_can_be_retried") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let (mut controller, mut operation) = fixture();
    let cloud = Ec2 {
        fail_boot: AtomicBool::new(true),
        fail_cleanup: AtomicBool::new(true),
        ..Default::default()
    };
    let error = controller
        .recover_prepared_ec2_destination(&mut operation, &cloud)
        .unwrap_err();
    let result = controller
        .finish_move_result(&mut operation, Err(error), &cloud)
        .unwrap();
    assert!(result.recovery.unwrap().contains("charges may continue"));
    let mut recovered = crate::database::load_move_operation(ID).unwrap().unwrap();
    assert!(matches!(
        recovered.prepared_destination.as_ref().unwrap().state,
        PreparedDestinationState::CleanupPending { .. }
    ));
    cloud.fail_cleanup.store(false, Ordering::SeqCst);
    controller
        .cleanup_prepared_move_destination(&mut recovered, &cloud)
        .unwrap();
    super::super::record_finished_move_recovery(&controller.state, &mut recovered).unwrap();
    let published = crate::database::load_session_record(ID)
        .unwrap()
        .unwrap()
        .last_error
        .unwrap();
    assert!(
        published.contains("EC2 destination cleaned up"),
        "{published}"
    );
    assert!(!published.contains("charges may continue"), "{published}");
    assert!(matches!(
        recovered.prepared_destination.unwrap().state,
        PreparedDestinationState::Released
    ));
    assert_eq!(
        crate::database::load_session_state(ID).unwrap(),
        Some(SessionState::Running)
    );
}

#[test]
fn insufficient_ec2_destination_space_fails_before_source_interruption() {
    if !isolated("insufficient_ec2_destination_space_fails_before_source_interruption") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let (controller, mut operation) = fixture();
    let cloud = Ec2 {
        low_disk: AtomicBool::new(true),
        ..Default::default()
    };
    controller
        .recover_prepared_ec2_destination(&mut operation, &cloud)
        .unwrap();
    let mut assessment = operation
        .workspace_transfer
        .as_ref()
        .unwrap()
        .assessment
        .clone();
    let error = controller
        .assess_prepared_destination(&operation, &mut assessment, &cloud)
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Destination needs approximately"),
        "{error:#}"
    );
    assert_eq!(
        crate::database::load_session_state(ID).unwrap(),
        Some(SessionState::Running)
    );
    controller
        .cleanup_prepared_move_destination(&mut operation, &cloud)
        .unwrap();
}

#[test]
fn destination_adoption_preserves_the_created_instance_and_handoff_identity() {
    if !isolated("destination_adoption_preserves_the_created_instance_and_handoff_identity") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let (mut controller, mut operation) = fixture();
    let cloud = Ec2::default();
    controller
        .recover_prepared_ec2_destination(&mut operation, &cloud)
        .unwrap();
    let archives = tempfile::tempdir().unwrap();
    operation.handoff = Some(write_network_checkpoint_archive(archives.path(), ID, 0));
    operation.phase = MovePhase::ResumingDestination;
    crate::database::save_move_operation(&operation).unwrap();
    controller.state.sessions.get_mut(ID).unwrap().state = SessionState::Provisioning;
    crate::database::save_session(&controller.state.sessions[ID]).unwrap();
    let plan = controller
        .adopt_prepared_ec2_destination(ID, None, &cloud)
        .unwrap()
        .unwrap();
    assert!(plan.commands.iter().all(|command| command.program != "aws"));
    let saved = crate::database::load_session_record(ID).unwrap().unwrap();
    assert_eq!(
        saved.target.as_ref(),
        operation.prepared_destination.as_ref().unwrap().target()
    );
    assert_eq!(saved.native_session_id.as_deref(), Some("native-session"));
    assert_eq!(saved.id, ID);
    assert!(matches!(
        crate::database::load_move_operation(ID)
            .unwrap()
            .unwrap()
            .prepared_destination
            .unwrap()
            .state,
        PreparedDestinationState::Adopted { .. }
    ));
}

// A destination an older daemon recorded as `Checked` skipped the IMDSv2
// requirement when it was prepared; adoption enforces it, whichever path led
// there.
#[test]
fn adopting_a_checked_destination_requires_imdsv2_on_its_instance() {
    if !isolated("adopting_a_checked_destination_requires_imdsv2_on_its_instance") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let (mut controller, mut operation) = fixture();
    controller
        .recover_prepared_ec2_destination(&mut operation, &Ec2::default())
        .unwrap();
    assert!(matches!(
        operation.prepared_destination.as_ref().unwrap().state,
        PreparedDestinationState::Checked { .. }
    ));
    let archives = tempfile::tempdir().unwrap();
    operation.handoff = Some(write_network_checkpoint_archive(archives.path(), ID, 0));
    operation.phase = MovePhase::ResumingDestination;
    crate::database::save_move_operation(&operation).unwrap();
    controller.state.sessions.get_mut(ID).unwrap().state = SessionState::Provisioning;
    crate::database::save_session(&controller.state.sessions[ID]).unwrap();

    // The instance it recorded allows IMDSv1.
    let cloud = Ec2 {
        imds_optional: AtomicBool::new(true),
        ..Default::default()
    };
    controller
        .adopt_prepared_ec2_destination(ID, None, &cloud)
        .unwrap()
        .unwrap();
    assert!(
        cloud
            .commands
            .lock()
            .unwrap()
            .iter()
            .any(|command| command.purpose == "require IMDSv2 on EC2 instance"),
        "an instance that allows IMDSv1 was adopted"
    );
    assert!(!cloud.imds_optional.load(Ordering::SeqCst));
}

#[test]
fn ec2_orphan_scan_respects_prepared_and_ambiguous_move_ownership() {
    if !isolated("ec2_orphan_scan_respects_prepared_and_ambiguous_move_ownership") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let (controller, mut operation) = fixture();
    let cloud = Ec2::default();
    controller
        .recover_prepared_ec2_destination(&mut operation, &cloud)
        .unwrap();
    struct Scan;
    impl CommandExecutor for Scan {
        fn execute(&self, command: &CommandSpec) -> Result<targets::CommandOutput> {
            let listing = command.purpose == "scan managed EC2 workers";
            Ok(targets::CommandOutput {
                status: if listing { 0 } else { 1 },
                stdout: if listing {
                    serde_json::to_vec(&serde_json::json!({"Reservations":[{"Instances":[{
                        "InstanceId":INSTANCE, "PublicIpAddress":"127.0.0.1",
                        "Tags":[{"Key":"dev.mj.managed", "Value":"true"},
                                {"Key":"dev.mj.session", "Value":ID}]
                    }]}]}))?
                } else {
                    Vec::new()
                },
                stderr: Vec::new(),
            })
        }
    }
    for state in [
        PreparedDestinationState::LaunchPending,
        PreparedDestinationState::Created {
            instance_id: INSTANCE.into(),
        },
        PreparedDestinationState::CleanupPending { instance_id: None },
        PreparedDestinationState::CleanupPending {
            instance_id: Some(INSTANCE.into()),
        },
    ] {
        operation.prepared_destination.as_mut().unwrap().state = state;
        crate::database::save_move_operation(&operation).unwrap();
        let scan = controller.scan_orphan_workers(&Scan, true);
        assert!(scan.warnings.is_empty(), "{scan:?}");
        assert!(
            scan.candidates.is_empty(),
            "a Move-owned resource became an orphan: {scan:?}"
        );
    }
    operation.prepared_destination.as_mut().unwrap().state = PreparedDestinationState::Released;
    crate::database::save_move_operation(&operation).unwrap();
    assert_eq!(
        controller.scan_orphan_workers(&Scan, true).candidates.len(),
        1
    );
}

#[test]
fn ec2_boot_interrupted_by_daemon_handoff_resumes_the_same_owned_instance() {
    if !isolated("ec2_boot_interrupted_by_daemon_handoff_resumes_the_same_owned_instance") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let (controller, mut operation) = fixture();
    let cloud = Ec2 {
        fail_boot: AtomicBool::new(true),
        handoff: AtomicBool::new(true),
        ..Default::default()
    };
    let error = controller
        .recover_prepared_ec2_destination(&mut operation, &cloud)
        .unwrap_err();
    assert!(error.to_string().contains("daemon handoff"));
    let mut recovered = crate::database::load_move_operation(ID).unwrap().unwrap();
    assert!(matches!(
        recovered.prepared_destination.as_ref().unwrap().state,
        PreparedDestinationState::Created { .. }
    ));
    assert_eq!(
        crate::database::load_session_state(ID).unwrap(),
        Some(SessionState::Running)
    );
    cloud.fail_boot.store(false, Ordering::SeqCst);
    cloud.handoff.store(false, Ordering::SeqCst);
    controller
        .recover_prepared_ec2_destination(&mut recovered, &cloud)
        .unwrap();
    assert_eq!(
        cloud
            .commands
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.purpose == "launch EC2 Move destination")
            .count(),
        0
    );
    assert_eq!(
        recovered.prepared_destination.unwrap().instance_id(),
        Some(INSTANCE)
    );
}

fn pending_launch_recorded_by_an_older_daemon(token: &str) -> PreparedMoveDestination {
    // The arguments an older daemon stored: no metadata options.
    PreparedMoveDestination {
        launch_args: [
            "--profile",
            "test",
            "--region",
            "us-east-1",
            "ec2",
            "run-instances",
            "--client-token",
            token,
            "--output",
            "json",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
        runtime: TargetRuntimeSettings::from(&ec2_target()),
        state: PreparedDestinationState::LaunchPending,
    }
}

#[test]
fn an_older_pending_launch_is_only_queried_and_imdsv2_is_enforced_on_the_instance() {
    if !isolated("an_older_pending_launch_is_only_queried_and_imdsv2_is_enforced_on_the_instance") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let (controller, mut operation) = fixture();
    let recorded = pending_launch_recorded_by_an_older_daemon("a-token-stored-by-an-older-daemon");
    operation.prepared_destination = Some(recorded.clone());
    crate::database::save_move_operation(&operation).unwrap();
    let cloud = Ec2 {
        // The accepted instance is a launch without the option.
        imds_optional: AtomicBool::new(true),
        ..Default::default()
    };
    controller
        .recover_prepared_ec2_destination(&mut operation, &cloud)
        .unwrap();
    let commands = cloud.commands.lock().unwrap();
    assert!(
        commands
            .iter()
            .any(|command| command.purpose == "reconcile EC2 Move launch acknowledgement")
    );
    assert_eq!(
        operation.prepared_destination.as_ref().unwrap().launch_args,
        recorded.launch_args
    );
    let purposes = commands
        .iter()
        .map(|command| command.purpose.as_str())
        .collect::<Vec<_>>();
    let position = |purpose: &str| purposes.iter().position(|p| *p == purpose).unwrap();
    // Changed in place, after the instance runs and before anything uses it.
    assert!(
        position("wait for EC2 session instance to run")
            < position("require IMDSv2 on EC2 instance")
    );
    assert!(position("require IMDSv2 on EC2 instance") < position("resolve EC2 session address"));
    assert!(!purposes.contains(&"terminate exact EC2 session instance"));
}

#[test]
fn a_created_destination_found_after_a_crash_is_checked_before_it_boots() {
    if !isolated("a_created_destination_found_after_a_crash_is_checked_before_it_boots") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let (controller, mut operation) = fixture();
    let mut created = pending_launch_recorded_by_an_older_daemon("token");
    created.state = PreparedDestinationState::Created {
        instance_id: INSTANCE.into(),
    };
    operation.prepared_destination = Some(created);
    crate::database::save_move_operation(&operation).unwrap();
    let cloud = Ec2 {
        imds_optional: AtomicBool::new(true),
        ..Default::default()
    };
    controller
        .recover_prepared_ec2_destination(&mut operation, &cloud)
        .unwrap();
    let commands = cloud.commands.lock().unwrap();
    assert!(
        !commands
            .iter()
            .any(|command| command.purpose == "launch EC2 Move destination"),
        "an instance that already exists is not launched again"
    );
    assert!(
        commands
            .iter()
            .any(|command| command.purpose == "require IMDSv2 on EC2 instance")
    );
    assert!(matches!(
        operation.prepared_destination.unwrap().state,
        PreparedDestinationState::Checked { .. }
    ));
}

#[test]
fn a_destination_that_cannot_require_imdsv2_fails_the_move_and_is_terminated() {
    if !isolated("a_destination_that_cannot_require_imdsv2_fails_the_move_and_is_terminated") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let (mut controller, mut operation) = fixture();
    let cloud = Ec2 {
        imds_optional: AtomicBool::new(true),
        fail_imds_enforcement: AtomicBool::new(true),
        ..Default::default()
    };
    let error = controller
        .recover_prepared_ec2_destination(&mut operation, &cloud)
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("could not require IMDSv2"),
        "{error:#}"
    );
    let result = controller
        .finish_move_result(&mut operation, Err(error), &cloud)
        .unwrap();
    assert_eq!(result.outcome, "failed");
    assert!(
        cloud
            .commands
            .lock()
            .unwrap()
            .iter()
            .any(|command| command.purpose == "terminate exact EC2 session instance")
    );
    assert!(matches!(
        operation.prepared_destination.unwrap().state,
        PreparedDestinationState::Released
    ));
}
