//! Compatibility recovery and cleanup for EC2 destinations accepted by older daemons.
//! This module cannot launch an instance.
use super::*;
use crate::targets::{self, CommandSpec};
use mj_core::config::TargetTemplate;
use mj_core::state::{
    PreparedDestinationState, PreparedMoveDestination, TargetLocator, TargetRuntimeSettings,
};

impl Controller {
    pub(super) fn recover_prepared_ec2_destination(
        &self,
        operation: &mut MoveOperation,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        executor.begin_resumable_work()?;
        let result = self.recover_prepared_ec2_destination_inner(operation, executor);
        let admission = executor.end_resumable_work("Move control transition");
        admission?;
        result
    }

    fn recover_prepared_ec2_destination_inner(
        &self,
        operation: &mut MoveOperation,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        let id = &operation.selection.session_id;
        let template = self
            .config
            .targets
            .get(
                operation
                    .selection
                    .target_template_id
                    .as_deref()
                    .context("Move target missing")?,
            )
            .context("EC2 Move target disappeared")?;
        ensure!(
            matches!(template, TargetTemplate::AwsEc2 { .. }),
            "Move preparation requires an EC2 target"
        );
        ensure!(
            !executor.cancellation_requested(),
            "Move cancelled before EC2 preparation"
        );
        let destination = operation
            .prepared_destination
            .as_ref()
            .filter(|destination| destination.owns_resource())
            .context(mj_core::config::LEGACY_EC2_TARGET_NOTICE)?
            .clone();
        if matches!(destination.state, PreparedDestinationState::LaunchPending) {
            // An old accepted Move can recover an existing resource by its
            // recorded identity. It must never replay a launch after retirement.
            let instance_id = pending_instance_id(operation, &destination, executor)?;
            operation.prepared_destination.as_mut().unwrap().state =
                PreparedDestinationState::Created { instance_id };
            crate::database::save_move_operation(operation)?;
        }
        if let PreparedDestinationState::Created { instance_id } = operation
            .prepared_destination
            .as_ref()
            .unwrap()
            .state
            .clone()
        {
            executor.notify_notice("Booting EC2 destination");
            let target =
                super::super::backend::existing_ec2_locator(template, instance_id, executor)?;
            let backend = prepared_backend(&target, &destination.runtime, id)?;
            let targets::TargetLocator::AwsEc2 { ssh, workspace, .. } = &backend else {
                unreachable!()
            };
            super::super::execute_checked(
                executor,
                targets::ssh_command(ssh, ["mkdir", "-p", workspace])
                    .purpose("create prepared EC2 workspace"),
            )?;
            targets::install_git_plan(targets::ExecutionBoundary::Ssh(ssh)).execute(executor)?;
            targets::install_rsync_plan(targets::ExecutionBoundary::Ssh(ssh)).execute(executor)?;
            operation.prepared_destination.as_mut().unwrap().state =
                PreparedDestinationState::Checked { target };
            crate::database::save_move_operation(operation)?;
        }
        ensure!(
            matches!(
                operation.prepared_destination.as_ref().unwrap().state,
                PreparedDestinationState::Checked { .. } | PreparedDestinationState::Adopted { .. }
            ),
            "EC2 destination cleanup is pending; source retained"
        );
        Ok(())
    }

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

    pub(in crate::controller) fn adopt_prepared_ec2_destination(
        &mut self,
        id: &str,
        github_token: Option<&str>,
        executor: &impl CommandExecutor,
    ) -> Result<Option<targets::CommandPlan>> {
        let Some(mut operation) = crate::database::load_move_operation(id)? else {
            return Ok(None);
        };
        if operation.phase != MovePhase::ResumingDestination {
            return Ok(None);
        }
        let Some(destination) = operation.prepared_destination.as_ref() else {
            return Ok(None);
        };
        let target = destination
            .target()
            .context("prepared EC2 destination is not available for adoption")?
            .clone();
        let runtime = destination.runtime.clone();
        let backend = prepared_backend(&target, &runtime, id)?;
        // One rule for every instance Mjolnir adopts: a destination an older
        // daemon recorded as checked never had IMDSv2 required, so it is
        // required here too. The check is idempotent and costs one describe
        // when the instance already requires it.
        if let targets::TargetLocator::AwsEc2 {
            profile,
            region,
            instance_id,
            ..
        } = &backend
        {
            super::super::backend::ensure_imdsv2(profile, region, instance_id, executor)?;
        }
        let bundle = self
            .move_destination_bundle(id)?
            .context("prepared EC2 Move bundle missing")?;
        let plan = targets::provision_on_locator_plan(&backend, id, &bundle, github_token)?;
        let session = self
            .state
            .sessions
            .get_mut(id)
            .context("EC2 Move session missing")?;
        session.target = Some(target.clone());
        session.target_runtime = Some(runtime);
        operation.prepared_destination.as_mut().unwrap().state =
            PreparedDestinationState::Adopted { target };
        crate::database::adopt_move_destination(&operation, session)?;
        Ok(Some(plan))
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

pub(super) fn prepared_backend(
    target: &TargetLocator,
    runtime: &TargetRuntimeSettings,
    id: &str,
) -> Result<targets::TargetLocator> {
    Ok(targets::TargetLocator::try_from(targets::RecordedTarget {
        locator: target,
        runtime: Some(runtime),
        session_id: id,
    })?)
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

#[cfg(all(test, unix))]
pub(super) mod tests;
