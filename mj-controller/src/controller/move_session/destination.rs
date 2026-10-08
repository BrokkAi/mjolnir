//! Durable preparation of an EC2 destination while the source worker remains live.
use super::*;
use crate::targets::{self, CommandSpec};
use mj_core::config::TargetTemplate;
use mj_core::state::{
    PreparedDestinationState, PreparedMoveDestination, TargetLocator, TargetRuntimeSettings,
};

impl Controller {
    pub(super) fn prepare_ec2_move_destination(
        &self,
        operation: &mut MoveOperation,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        executor.begin_resumable_move_work()?;
        let result = self.prepare_ec2_move_destination_inner(operation, executor);
        let admission = executor.end_resumable_move_work();
        admission?;
        result
    }

    fn prepare_ec2_move_destination_inner(
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
        let fresh_attempt = operation
            .prepared_destination
            .as_ref()
            .is_none_or(|d| matches!(d.state, PreparedDestinationState::Released));
        if fresh_attempt {
            let backend = super::super::backend::backend_target(
                template,
                operation.selection.resource_allocation.as_ref(),
                super::super::backend::ContainerOverrides::for_session(&self.state.sessions[id]),
            )?;
            let targets::TargetTemplate::AwsEc2(mut aws) = backend else {
                bail!("EC2 Move backend mismatch");
            };
            let version = super::super::execute_checked(
                executor,
                CommandSpec::new(
                    "aws",
                    [
                        "--profile".into(),
                        aws.profile.clone(),
                        "--region".into(),
                        aws.region.clone(),
                        "ec2".into(),
                        "describe-launch-template-versions".into(),
                        if aws.launch_template.starts_with("lt-") {
                            "--launch-template-id"
                        } else {
                            "--launch-template-name"
                        }
                        .into(),
                        aws.launch_template.clone(),
                        "--versions".into(),
                        aws.launch_template_version
                            .clone()
                            .unwrap_or_else(|| "$Default".into()),
                        "--query".into(),
                        "LaunchTemplateVersions[0].{LaunchTemplateId:LaunchTemplateId,VersionNumber:VersionNumber}".into(),
                        "--output".into(),
                        "json".into(),
                    ],
                )
                .purpose("resolve immutable EC2 launch template version"),
            )?;
            let resolved: serde_json::Value = serde_json::from_slice(&version.stdout)
                .context("parse immutable EC2 launch template identity")?;
            let template_id = resolved
                .get("LaunchTemplateId")
                .and_then(serde_json::Value::as_str)
                .filter(|id| id.starts_with("lt-"))
                .context("EC2 launch template omitted immutable template ID")?;
            let version = resolved
                .get("VersionNumber")
                .and_then(serde_json::Value::as_u64)
                .context("EC2 launch template omitted numeric version")?;
            aws.launch_template = template_id.to_owned();
            aws.launch_template_version = Some(version.to_string());
            let mut command = targets::ec2_launch_command(&aws, id)?;
            let token = digest(&(&operation.operation_id, new_command_id("ec2-attempt")?))?;
            command.args.extend([
                "--client-token".into(),
                token,
                "--min-count".into(),
                "1".into(),
                "--max-count".into(),
                "1".into(),
            ]);
            operation.prepared_destination = Some(PreparedMoveDestination {
                launch_args: command.args,
                runtime: TargetRuntimeSettings::from(template),
                state: PreparedDestinationState::LaunchPending,
            });
            crate::database::save_move_operation(operation)?;
        }
        let destination = operation.prepared_destination.as_ref().unwrap().clone();
        if matches!(destination.state, PreparedDestinationState::LaunchPending) {
            executor.notify_notice("Creating EC2 destination");
            let output = executor.execute(
                &CommandSpec::new("aws", destination.launch_args.clone())
                    .purpose("launch EC2 Move destination")
                    .stage(ProvisionStage::Provisioning),
            )?;
            if output.status != 0 {
                // Only a first-call refusal proves this attempt was never accepted.
                // A refused retry says nothing about a previously lost response.
                if fresh_attempt && launch_was_refused(&output.stderr) {
                    operation.prepared_destination.as_mut().unwrap().state =
                        PreparedDestinationState::Released;
                    crate::database::save_move_operation(operation)?;
                }
                bail!(
                    "EC2 Move launch failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            let json: serde_json::Value =
                serde_json::from_slice(&output.stdout).context("parse EC2 Move launch response")?;
            let instance_id = json
                .pointer("/Instances/0/InstanceId")
                .and_then(serde_json::Value::as_str)
                .context("EC2 Move launch omitted instance ID")?
                .to_owned();
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
                super::super::backend::ec2_locator_after_launch(template, instance_id, executor)?;
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
            instance_id = Some(ids[0].to_owned());
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

fn launch_was_refused(stderr: &[u8]) -> bool {
    let detail = String::from_utf8_lossy(stderr);
    [
        "UnauthorizedOperation",
        "AuthFailure",
        "InvalidParameterValue",
        "InvalidParameterCombination",
        "InsufficientInstanceCapacity",
        "InstanceLimitExceeded",
        "InvalidLaunchTemplateName.NotFoundException",
        "InvalidLaunchTemplateId.NotFound",
    ]
    .iter()
    .any(|code| detail.contains(&format!("An error occurred ({code})")))
}

#[cfg(all(test, unix))]
pub(super) mod tests;
