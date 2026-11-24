use super::*;
use mj_core::targets::SshTrafficClass;

/// Clone/bootstrap commands for AWS once the exact instance ID and address are known.
pub fn provision_on_locator_plan(
    locator: &TargetLocator,
    session_id: &str,
    bundle: &ProjectBundleSpec,
    github_token: Option<&str>,
) -> Result<CommandPlan> {
    bundle.validate()?;
    verify_locator(locator, session_id)?;
    ensure!(
        !is_borrowed(locator),
        "refusing to operate on a target borrowed from another session; act on the owning session instead"
    );
    let TargetLocator::AwsEc2 { ssh, workspace, .. } = locator else {
        bail!("post-launch provisioning is only required for AWS");
    };
    let mut commands = vec![
        ssh_command(SshTrafficClass::Control, ssh, ["mkdir", "-p", workspace])
            .purpose("create EC2 session workspace")
            .stage(ProvisionStage::Cloning),
    ];
    commands.extend(install_git_plan(ExecutionBoundary::Ssh(ssh)).commands);
    commands.extend(clone_commands(bundle, workspace, github_token, |args| {
        ssh_command_owned(SshTrafficClass::Bulk, ssh, args)
    }));
    Ok(CommandPlan {
        description: format!("initialize EC2 session {session_id}"),
        commands,
    })
}

/// Reconstruct repositories without creating or retiring their target.
pub fn recover_checkout_plan(
    locator: &TargetLocator,
    session_id: &str,
    bundle: &ProjectBundleSpec,
    workspace: &str,
    github_token: Option<&str>,
    existing_repositories: &std::collections::BTreeSet<String>,
) -> Result<CommandPlan> {
    bundle.validate()?;
    verify_locator(locator, session_id)?;
    ensure!(
        !is_borrowed(locator),
        "recover the owning session's checkout first"
    );
    let mut missing = bundle.clone();
    missing
        .repositories
        .retain(|repository| !existing_repositories.contains(&repository.destination));
    Ok(CommandPlan {
        description: "reconstruct missing checkout in staging".into(),
        commands: clone_commands(&missing, workspace, github_token, |args| {
            locator_command(SshTrafficClass::Bulk, locator, args)
        }),
    })
}

pub fn reconnect_plan(locator: &TargetLocator, session_id: &str) -> Result<CommandPlan> {
    relay_proxy_plan(locator, session_id, SshTrafficClass::Control)
}

/// Bulk relay transactions (project memory, skills, attachments and live
/// baselines) share this lane so large frames do not queue ahead of Control RPCs.
pub fn bulk_relay_command(locator: &TargetLocator, session_id: &str) -> Result<CommandSpec> {
    relay_proxy_plan(locator, session_id, SshTrafficClass::Bulk)?
        .commands
        .into_iter()
        .next()
        .context("bulk relay plan is empty")
}

/// The Bulk relay connection for a project-memory transaction.
pub fn project_memory_relay_command(
    locator: &TargetLocator,
    session_id: &str,
) -> Result<CommandSpec> {
    bulk_relay_command(locator, session_id)
}

fn relay_proxy_plan(
    locator: &TargetLocator,
    session_id: &str,
    class: SshTrafficClass,
) -> Result<CommandPlan> {
    verify_locator(locator, session_id)?;
    let root = worker_root(locator, session_id)?;
    let binary = format!("{root}/hel");
    let command = locator_command(
        class,
        locator,
        vec![
            binary,
            "worker".into(),
            "proxy".into(),
            "--root".into(),
            root,
        ],
    )
    .purpose("connect to Mjolnir worker")
    .stage(ProvisionStage::Starting);
    Ok(CommandPlan {
        description: format!("reconnect Mjolnir session {session_id}"),
        commands: vec![command],
    })
}

/// Describe safe recovery for a container that belongs to an active
/// session. The inspect command is deliberately separate from `exec`: a host
/// crash can leave the container present but stopped, where `exec` cannot
/// distinguish that state from other transport failures.
pub fn target_recovery_plan(
    locator: &TargetLocator,
    session_id: &str,
) -> Result<Option<TargetRecoveryPlan>> {
    verify_locator(locator, session_id)?;
    // Nothing to recover at the target level for a borrowed worker; the
    // owning session recovers the target. Reporting no plan keeps the child's
    // liveness probe and worker restart working.
    if is_borrowed(locator) {
        return Ok(None);
    }
    let Some(endpoint) = locator.container_endpoint() else {
        return Ok(None);
    };
    let container_id = endpoint.container_id;
    let (exists, inspect, start) = match endpoint.engine {
        "podman" => (
            CommandSpec::new("podman", ["container", "exists", container_id])
                .purpose("check for Mjolnir session container"),
            CommandSpec::new("podman", ["container", "inspect", container_id])
                .purpose("inspect Mjolnir session container"),
            CommandSpec::new("podman", ["start", container_id])
                .purpose("start stopped Mjolnir session container"),
        ),
        "docker" => (
            CommandSpec::new(
                "sh",
                [
                    "-c",
                    "docker container inspect \"$1\" >/dev/null 2>&1 && exit 0; docker info >/dev/null 2>&1 && exit 1; exit 125",
                    "mj-docker-exists",
                    container_id,
                ],
            )
            .purpose("check for Mjolnir Docker session container"),
            CommandSpec::new("docker", ["container", "inspect", container_id])
                .purpose("inspect Mjolnir Docker session container"),
            CommandSpec::new("docker", ["start", container_id])
                .purpose("start stopped Mjolnir Docker session container"),
        ),
        "container" => return Ok(None),
        MICROVM_ENGINE => (
            engine_command(MICROVM_ENGINE, ["container", "exists", container_id])
                .purpose("check for remote Mjolnir session sandbox"),
            engine_command(MICROVM_ENGINE, ["container", "inspect", container_id])
                .purpose("inspect remote Mjolnir session sandbox"),
            engine_command(MICROVM_ENGINE, ["start", container_id])
                .purpose("start stopped remote Mjolnir session sandbox"),
        ),
        _ => unreachable!("unknown container engine"),
    };
    let [exists, inspect, start] = [exists, inspect, start]
        .map(|command| endpoint.host_command(SshTrafficClass::Control, command));
    Ok(Some(TargetRecoveryPlan {
        exists,
        inspect,
        start,
        session_id: session_id.to_owned(),
    }))
}

/// Start a confirmed stopped container target and verify it reached `running`.
/// Missing or foreign containers, transport failures, and transitional states
/// fail without running the start command.
pub fn ensure_recovery_target_running(
    executor: &impl CommandExecutor,
    plan: Option<&TargetRecoveryPlan>,
) -> Result<TargetRecoveryOutcome> {
    let Some(plan) = plan else {
        return Ok(TargetRecoveryOutcome::NotRequired);
    };
    let existence = executor
        .execute(&plan.exists)
        .context("check whether container session target exists")?;
    match existence.status {
        0 => {}
        // `podman container exists` deliberately reserves 1 for absence and
        // uses 125 for invocation or storage failures. SSH preserves the
        // remote exit status, so this contract also covers remote Podman.
        1 => return Ok(TargetRecoveryOutcome::Missing),
        _ => {
            checked_command_output(&plan.exists, existence)
                .context("check whether container session target exists")?;
            unreachable!("a successful checked command has status zero");
        }
    }
    let status = inspect_recovery_target(executor, plan)?;
    match status.as_str() {
        "running" => Ok(TargetRecoveryOutcome::AlreadyRunning),
        "created" | "initialized" | "stopped" | "exited" => {
            let output = executor.execute(&plan.start)?;
            checked_command_output(&plan.start, output)
                .context("start confirmed stopped container session target")?;
            let after = inspect_recovery_target(executor, plan)
                .context("verify container session target after starting it")?;
            ensure!(
                after == "running",
                "container session target reported {after:?} after start"
            );
            Ok(TargetRecoveryOutcome::Started)
        }
        "paused" | "removing" | "stopping" | "unknown" => {
            bail!("refusing to start container session target in {status:?} state")
        }
        _ => bail!("container session target reported unexpected state {status:?}"),
    }
}

pub(super) fn inspect_recovery_target(
    executor: &impl CommandExecutor,
    plan: &TargetRecoveryPlan,
) -> Result<String> {
    let output = executor.execute(&plan.inspect)?;
    let output = checked_command_output(&plan.inspect, output)
        .context("inspect container session target for recovery")?;
    let values: Vec<serde_json::Value> =
        serde_json::from_slice(&output.stdout).context("parse container target inspection")?;
    ensure!(
        values.len() == 1,
        "container inspection returned {} targets instead of one",
        values.len()
    );
    let target = &values[0];
    let labels = target
        .pointer("/Config/Labels")
        .and_then(serde_json::Value::as_object)
        .context("container session target has no ownership labels")?;
    ensure!(
        labels
            .get(MANAGED_LABEL)
            .and_then(serde_json::Value::as_str)
            == Some("true"),
        "refusing to start a container target Mjolnir does not own"
    );
    ensure!(
        labels
            .get(SESSION_LABEL)
            .and_then(serde_json::Value::as_str)
            == Some(plan.session_id.as_str()),
        "refusing to start a container target owned by another session"
    );
    target
        .pointer("/State/Status")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .context("container session target inspection has no state")
}
