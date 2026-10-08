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
    crate::database::save_move_operation(&operation).unwrap();
    (Controller { config, state }, operation)
}

#[derive(Default)]
struct Ec2 {
    commands: Mutex<Vec<CommandSpec>>,
    tokens: Mutex<std::collections::BTreeSet<String>>,
    lose_ack: AtomicBool,
    refuse_launch: AtomicBool,
    fail_boot: AtomicBool,
    fail_cleanup: AtomicBool,
    low_disk: AtomicBool,
    handoff: AtomicBool,
    resumable: AtomicBool,
}

impl CommandExecutor for Ec2 {
    fn begin_resumable_move_work(&self) -> Result<()> {
        self.resumable.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn end_resumable_move_work(&self) -> Result<()> {
        self.resumable.store(false, Ordering::SeqCst);
        ensure!(
            !self.handoff.load(Ordering::SeqCst),
            "daemon handoff interrupted preparation"
        );
        Ok(())
    }

    fn execute(&self, command: &CommandSpec) -> Result<targets::CommandOutput> {
        self.commands.lock().unwrap().push(command.clone());
        let mut stdout = Vec::new();
        match command.purpose.as_str() {
            "resolve immutable EC2 launch template version" => {
                stdout =
                    br#"{"LaunchTemplateId":"lt-0123456789abcdef0","VersionNumber":7}"#.to_vec()
            }
            "launch EC2 Move destination" => {
                if self.refuse_launch.load(Ordering::SeqCst) {
                    return Ok(targets::CommandOutput {
                        status: 1,
                        stdout: Vec::new(),
                        stderr:
                            b"An error occurred (UnauthorizedOperation) when calling RunInstances"
                                .to_vec(),
                    });
                }
                self.tokens
                    .lock()
                    .unwrap()
                    .insert(argument(&command.args, "--client-token")?.into());
                assert!(matches!(
                    crate::database::load_move_operation(ID)?
                        .unwrap()
                        .prepared_destination
                        .unwrap()
                        .state,
                    PreparedDestinationState::LaunchPending
                ));
                if self.lose_ack.swap(false, Ordering::SeqCst) {
                    bail!("launch accepted; acknowledgement lost");
                }
                stdout = format!(r#"{{"Instances":[{{"InstanceId":"{INSTANCE}"}}]}}"#).into_bytes();
            }
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
fn ec2_preparation_persists_launch_before_boot_and_reuses_the_checked_instance() {
    if !isolated("ec2_preparation_persists_launch_before_boot_and_reuses_the_checked_instance") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let (mut controller, mut operation) = fixture();
    let TargetTemplate::AwsEc2 {
        launch_template, ..
    } = controller.config.targets.get_mut("ec2").unwrap()
    else {
        unreachable!()
    };
    *launch_template = "named-move-template".into();
    let cloud = Ec2::default();
    controller
        .prepare_ec2_move_destination(&mut operation, &cloud)
        .unwrap();
    // Reconstruct the operation, as a replacement daemon does.
    let mut recovered = crate::database::load_move_operation(ID).unwrap().unwrap();
    controller
        .prepare_ec2_move_destination(&mut recovered, &cloud)
        .unwrap();
    assert_eq!(cloud.tokens.lock().unwrap().len(), 1);
    let commands = cloud.commands.lock().unwrap();
    assert!(commands.iter().any(|command| command.purpose
        == "resolve immutable EC2 launch template version"
        && argument(&command.args, "--launch-template-name").unwrap() == "named-move-template"));
    let launches: Vec<_> = commands
        .iter()
        .filter(|c| c.purpose == "launch EC2 Move destination")
        .collect();
    assert_eq!(launches.len(), 1);
    assert!(
        launches[0]
            .args
            .iter()
            .any(|a| a.contains("LaunchTemplateId=lt-0123456789abcdef0,Version=7"))
    );
    assert_eq!(
        argument(&launches[0].args, "--client-token").unwrap().len(),
        64
    );
    assert_eq!(
        crate::database::load_session_record(ID).unwrap().unwrap(),
        controller.state.sessions[ID]
    );
}

#[test]
fn a_lost_ec2_launch_acknowledgement_cannot_create_a_second_instance() {
    if !isolated("a_lost_ec2_launch_acknowledgement_cannot_create_a_second_instance") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let (controller, mut operation) = fixture();
    let cloud = Ec2 {
        lose_ack: AtomicBool::new(true),
        ..Default::default()
    };
    assert!(
        controller
            .prepare_ec2_move_destination(&mut operation, &cloud)
            .is_err()
    );
    let mut recovered = crate::database::load_move_operation(ID).unwrap().unwrap();
    controller
        .prepare_ec2_move_destination(&mut recovered, &cloud)
        .unwrap();
    assert_eq!(
        cloud.tokens.lock().unwrap().len(),
        1,
        "both launches use the same idempotent attempt"
    );
    assert_eq!(
        recovered.prepared_destination.unwrap().instance_id(),
        Some(INSTANCE)
    );
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
        .prepare_ec2_move_destination(&mut operation, &cloud)
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
    let cloud = Ec2 {
        lose_ack: AtomicBool::new(true),
        ..Default::default()
    };
    assert!(
        controller
            .prepare_ec2_move_destination(&mut operation, &cloud)
            .is_err()
    );
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
        1
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
        .prepare_ec2_move_destination(&mut operation, &cloud)
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
        .prepare_ec2_move_destination(&mut operation, &cloud)
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
        .prepare_ec2_move_destination(&mut operation, &cloud)
        .unwrap();
    let archives = tempfile::tempdir().unwrap();
    operation.handoff = Some(write_network_checkpoint_archive(archives.path(), ID, 0));
    operation.phase = MovePhase::ResumingDestination;
    crate::database::save_move_operation(&operation).unwrap();
    controller.state.sessions.get_mut(ID).unwrap().state = SessionState::Provisioning;
    crate::database::save_session(&controller.state.sessions[ID]).unwrap();
    let plan = controller
        .adopt_prepared_ec2_destination(ID, None)
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
    assert_eq!(cloud.tokens.lock().unwrap().len(), 1);
}

#[test]
fn ambiguous_ec2_launch_errors_retain_ownership_for_reconciliation() {
    assert!(!launch_was_refused(
        b"connection closed before launch response"
    ));
    assert!(!launch_was_refused(
        b"An error occurred (InternalError) when calling RunInstances"
    ));
    assert!(launch_was_refused(
        b"An error occurred (UnauthorizedOperation) when calling RunInstances"
    ));
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
        .prepare_ec2_move_destination(&mut operation, &cloud)
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
fn an_ec2_retry_after_confirmed_cleanup_uses_a_new_launch_attempt() {
    if !isolated("an_ec2_retry_after_confirmed_cleanup_uses_a_new_launch_attempt") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let (mut controller, mut operation) = fixture();
    let cloud = Ec2 {
        fail_boot: AtomicBool::new(true),
        ..Default::default()
    };
    let error = controller
        .prepare_ec2_move_destination(&mut operation, &cloud)
        .unwrap_err();
    controller
        .finish_move_result(&mut operation, Err(error), &cloud)
        .unwrap();
    cloud.fail_boot.store(false, Ordering::SeqCst);
    let mut recovered = crate::database::load_move_operation(ID).unwrap().unwrap();
    controller
        .prepare_ec2_move_destination(&mut recovered, &cloud)
        .unwrap();
    assert_eq!(cloud.tokens.lock().unwrap().len(), 2);
    let commands = cloud.commands.lock().unwrap();
    let terminated = commands
        .iter()
        .position(|c| c.purpose == "confirm EC2 Move destination termination")
        .unwrap();
    let retried = commands
        .iter()
        .rposition(|c| c.purpose == "launch EC2 Move destination")
        .unwrap();
    assert!(terminated < retried);
    assert_eq!(
        crate::database::load_session_state(ID).unwrap(),
        Some(SessionState::Running)
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
        .prepare_ec2_move_destination(&mut operation, &cloud)
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
        .prepare_ec2_move_destination(&mut recovered, &cloud)
        .unwrap();
    assert_eq!(
        cloud
            .commands
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.purpose == "launch EC2 Move destination")
            .count(),
        1
    );
    assert_eq!(
        recovered.prepared_destination.unwrap().instance_id(),
        Some(INSTANCE)
    );
}

#[test]
fn a_refused_ec2_retry_preserves_ownership_of_a_previously_accepted_launch() {
    if !isolated("a_refused_ec2_retry_preserves_ownership_of_a_previously_accepted_launch") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let (mut controller, mut operation) = fixture();
    let cloud = Ec2 {
        lose_ack: AtomicBool::new(true),
        ..Default::default()
    };
    assert!(
        controller
            .prepare_ec2_move_destination(&mut operation, &cloud)
            .is_err()
    );
    let mut recovered = crate::database::load_move_operation(ID).unwrap().unwrap();
    cloud.refuse_launch.store(true, Ordering::SeqCst);
    let error = controller
        .prepare_ec2_move_destination(&mut recovered, &cloud)
        .unwrap_err();
    assert!(matches!(
        recovered.prepared_destination.as_ref().unwrap().state,
        PreparedDestinationState::LaunchPending
    ));
    controller
        .finish_move_result(&mut recovered, Err(error), &cloud)
        .unwrap();
    assert!(matches!(
        recovered.prepared_destination.unwrap().state,
        PreparedDestinationState::Released
    ));
    assert_eq!(
        cloud
            .commands
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.purpose == "terminate exact EC2 session instance")
            .count(),
        1
    );
    assert_eq!(cloud.tokens.lock().unwrap().len(), 1);
    assert_eq!(
        crate::database::load_session_state(ID).unwrap(),
        Some(SessionState::Running)
    );
}

#[test]
fn a_first_ec2_launch_refusal_releases_an_attempt_without_cleanup() {
    if !isolated("a_first_ec2_launch_refusal_releases_an_attempt_without_cleanup") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let (mut controller, mut operation) = fixture();
    let cloud = Ec2 {
        refuse_launch: AtomicBool::new(true),
        ..Default::default()
    };
    let error = controller
        .prepare_ec2_move_destination(&mut operation, &cloud)
        .unwrap_err();
    controller
        .finish_move_result(&mut operation, Err(error), &cloud)
        .unwrap();
    assert!(matches!(
        operation.prepared_destination.unwrap().state,
        PreparedDestinationState::Released
    ));
    assert!(cloud.tokens.lock().unwrap().is_empty());
    assert!(
        cloud
            .commands
            .lock()
            .unwrap()
            .iter()
            .all(|c| c.purpose != "terminate exact EC2 session instance"
                && c.purpose != "reconcile EC2 Move launch acknowledgement")
    );
}
