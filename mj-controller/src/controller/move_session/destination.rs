//! Cleanup of a recorded EC2 destination; no provisioning or Move recovery.
use super::*;
use crate::targets::{self, CommandSpec};
use mj_core::state::{PreparedDestinationState, PreparedMoveDestination, TargetRuntimeSettings};

impl Controller {
    /// One bounded cleanup attempt; the daemon retries durable failures with backoff.
    pub fn cleanup_prepared_move_destination(
        &self,
        operation: &mut MoveOperation,
        executor: &(impl CommandExecutor + Sync),
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
        let environment = if let PreparedDestinationState::Adopted { target } = &destination.state {
            crate::database::worker_environments(&operation.selection.session_id)?
                .into_iter()
                .find(|environment| {
                    environment
                        .placement
                        .target
                        .as_ref()
                        .is_some_and(|stored| stored.same_resource(target))
                })
                .context("adopted Move destination has no durable environment owner")?
        } else {
            crate::database::prepared_move_environment(&operation.operation_id)?
                .context("prepared Move destination has no durable environment owner")?
        };
        super::super::environment::retire_owned_environment(
            environment,
            &self.config,
            executor,
            super::super::BranchDisposition::Keep,
            super::super::CheckoutDisposition::Remove,
        )?;
        operation.prepared_destination.as_mut().unwrap().state = PreparedDestinationState::Released;
        crate::database::save_move_operation(operation)?;
        Ok(())
    }
}

pub(in crate::controller) fn destroy_prepared_environment(
    authority: &super::super::environment::Reclamation,
    session_id: &str,
    destination: &PreparedMoveDestination,
    executor: &impl CommandExecutor,
) -> Result<()> {
    ensure!(
        destination.owns_resource()
            && !matches!(destination.state, PreparedDestinationState::Adopted { .. }),
        "adopted destinations must be reclaimed through their worker environment"
    );
    let instance_id = match destination.instance_id() {
        Some(id) => id.to_owned(),
        None => pending_instance_id(session_id, destination, executor)?,
    };
    authority.record_prepared_instance(&instance_id)?;
    let (profile, region) = aws_access(&destination.runtime)?;
    super::super::execute_checked(
        executor,
        targets::terminate_ec2_instance_command(profile, region, &instance_id)?,
    )?;
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
    Ok(())
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
    session_id: &str,
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
                &format!("Name=tag:dev.mj.session,Values={}", session_id),
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
