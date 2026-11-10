//! Cleanup of a recorded EC2 destination; no provisioning or Move recovery.
use super::*;
use crate::targets::{self, CommandSpec};
use mj_core::state::{PreparedDestinationState, PreparedMoveDestination, TargetRuntimeSettings};

impl Controller {
    /// One bounded cleanup attempt; the daemon retries durable failures with backoff.
    pub fn cleanup_prepared_move_destination(
        &self,
        operation: &mut MoveOperation,
        executor: &impl CommandExecutor,
    ) -> Result<()> {
        let cleanup = MoveCleanupExecutor(executor);
        let executor = &cleanup;
        let Some(destination) = operation
            .prepared_destination
            .as_ref()
            .filter(|d| d.owns_resource())
            .cloned()
        else {
            return Ok(());
        };
        let mut instance_id = destination.instance_id().map(str::to_owned);
        operation.prepared_destination.as_mut().unwrap().state =
            PreparedDestinationState::CleanupPending {
                instance_id: instance_id.clone(),
            };
        crate::database::save_move_operation(operation)?;
        if instance_id.is_none() {
            instance_id = Some(pending_instance_id(operation, &destination, executor)?);
            operation.prepared_destination.as_mut().unwrap().state =
                PreparedDestinationState::CleanupPending {
                    instance_id: instance_id.clone(),
                };
            crate::database::save_move_operation(operation)?;
        }
        let instance_id = instance_id.unwrap();
        let (profile, region) = aws_access(&destination.runtime)?;
        super::super::execute_checked(
            executor,
            targets::terminate_ec2_instance_command(profile, region, &instance_id)?,
        )?;
        // Confirm termination before releasing durable resource ownership.
        super::super::execute_checked(
            executor,
            CommandSpec::new(
                "aws",
                [
                    "--profile",
                    profile,
                    "--region",
                    region,
                    "ec2",
                    "wait",
                    "instance-terminated",
                    "--instance-ids",
                    &instance_id,
                ],
            )
            .purpose("confirm EC2 Move destination termination"),
        )?;
        operation.prepared_destination.as_mut().unwrap().state = PreparedDestinationState::Released;
        crate::database::save_move_operation(operation)?;
        Ok(())
    }
}

struct MoveCleanupExecutor<'a, E>(&'a E);

impl<E: CommandExecutor> CommandExecutor for MoveCleanupExecutor<'_, E> {
    fn execute(&self, command: &CommandSpec) -> Result<targets::CommandOutput> {
        self.0.execute_cleanup(command)
    }
}

fn argument<'a>(args: &'a [String], key: &str) -> Result<&'a str> {
    args.windows(2)
        .find(|a| a[0] == key)
        .map(|a| a[1].as_str())
        .with_context(|| format!("persisted EC2 launch lacks {key}"))
}

fn aws_access(runtime: &TargetRuntimeSettings) -> Result<(&str, &str)> {
    let mj_core::state::TargetConnection::Aws {
        profile, region, ..
    } = &runtime.connection
    else {
        bail!("prepared destination has no AWS access settings");
    };
    Ok((profile, region))
}

fn pending_instance_id(
    operation: &MoveOperation,
    destination: &PreparedMoveDestination,
    executor: &impl CommandExecutor,
) -> Result<String> {
    let token = argument(&destination.launch_args, "--client-token")?;
    let (profile, region) = aws_access(&destination.runtime)?;
    let output = super::super::execute_checked(
        executor,
        CommandSpec::new(
            "aws",
            [
                "--profile",
                profile,
                "--region",
                region,
                "ec2",
                "describe-instances",
                "--filters",
                &format!("Name=client-token,Values={token}"),
                &format!(
                    "Name=tag:dev.mj.session,Values={}",
                    operation.selection.session_id
                ),
                "Name=tag:dev.mj.managed,Values=true",
                "--output",
                "json",
            ],
        )
        .purpose("reconcile EC2 Move launch acknowledgement"),
    )?;
    let json: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let ids: Vec<&str> = json
        .get("Reservations")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|r| {
            r.get("Instances")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter_map(|i| i.get("InstanceId").and_then(serde_json::Value::as_str))
        .collect();
    ensure!(
        ids.len() == 1,
        "EC2 launch cleanup cannot yet resolve a unique instance for token {token}; automatic cleanup will retry"
    );
    Ok(ids[0].to_owned())
}
