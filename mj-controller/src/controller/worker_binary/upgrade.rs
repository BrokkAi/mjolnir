use super::*;
use mj_core::hex::lower_hex;

/// Replace `{worker_root}/hel` with the controller's current worker binary.
///
/// Checkpoint export starts that path as a new process. A live daemon already
/// has the previous inode mapped, so this does not restart it. Writing through
/// `hel.next` and renaming avoids `ETXTBSY` on a running image.
pub(in crate::controller) fn replace_installed_worker_binary(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    session_id: &str,
    worker_binary: &Path,
) -> Result<()> {
    let plan = installed_worker_binary_replacement_plan(locator, session_id, worker_binary)?;
    for command in plan.commands {
        execute_checked(executor, command)?;
    }
    Ok(())
}

/// Upload without changing the executable path used by the running worker or
/// its sidecars. Promotion happens only while an idle reservation is held.
pub(in crate::controller) fn stage_worker_binary_for_upgrade(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    session_id: &str,
    worker_binary: &Path,
) -> Result<()> {
    worker_binary_replacement_plan(locator, session_id, worker_binary, "hel.prepared")?
        .execute(executor)?;
    Ok(())
}

pub(in crate::controller) fn install_staged_worker_binary(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    session_id: &str,
) -> Result<()> {
    let root = targets::worker_root(locator, session_id)?;
    execute_checked(
        executor,
        targets::locator_command(
            locator,
            vec![
                "mv".into(),
                "-f".into(),
                "--".into(),
                format!("{root}/hel.prepared"),
                format!("{root}/hel"),
            ],
        )
        .purpose("install the prepared Mjolnir worker"),
    )?;
    Ok(())
}

pub(in crate::controller) fn replace_installed_worker_launch_config(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    session_id: &str,
    launch: &WorkerLaunchConfig,
) -> Result<()> {
    let plan = worker_launch_refresh_plan(locator, session_id, launch)?;
    for command in plan.replace.commands {
        execute_checked(executor, command)?;
    }
    Ok(())
}

/// Prepare the exact managed harness using the current worker binary. Remote
/// targets receive a separately staged copy; local bare targets run the binary
/// directly with a private launch config. The running worker is not stopped or
/// replaced, so any failure here leaves the quiet session attachable on its
/// previous build.
pub(in crate::controller) fn prepare_managed_harness_for_upgrade(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    session_id: &str,
    worker_binary: &Path,
    launch: &WorkerLaunchConfig,
) -> Result<()> {
    if launch.harness_runtime != HarnessRuntimePolicy::Managed {
        return Ok(());
    }
    let worker_root = targets::worker_root(locator, session_id)?;
    let staging_root = format!("{worker_root}/harness-prepare");
    let staging_binary = format!("{staging_root}/hel");
    let staging_config = format!("{staging_root}/launch.json");
    let staging = tempfile::tempdir().context("create managed harness upgrade staging")?;
    let local_config = staging.path().join("launch.json");
    launch.write(&local_config)?;

    // Local bare workers already share the controller's filesystem. Running
    // the current binary against a private launch config is enough to prepare
    // the cache, and leaves the live worker root completely untouched.
    if matches!(locator, targets::TargetLocator::LocalBare { .. }) {
        execute_checked(
            executor,
            CommandSpec::new(
                worker_binary.to_string_lossy().into_owned(),
                [
                    "worker".to_owned(),
                    "prepare-harness".to_owned(),
                    "--config".to_owned(),
                    local_config.to_string_lossy().into_owned(),
                ],
            )
            .purpose("prepare exact managed harness"),
        )?;
        return Ok(());
    }

    let ssh = match locator {
        targets::TargetLocator::AwsEc2 { ssh, .. }
        | targets::TargetLocator::SshBare { ssh, .. } => ssh,
        _ => bail!("managed harness policy requires a local bare, SSH-bare, or EC2 target"),
    };
    let result = (|| {
        execute_checked(
            executor,
            crate::targets::ssh_command(ssh, ["rm", "-rf", "--", &staging_root])
                .purpose("clear managed harness preparation staging"),
        )?;
        execute_checked(
            executor,
            crate::targets::ssh_command(ssh, ["mkdir", "-p", &staging_root])
                .purpose("create managed harness preparation staging"),
        )?;
        execute_checked(
            executor,
            crate::targets::scp_upload(ssh, worker_binary, &staging_binary, false)
                .purpose("stage current worker for managed harness preparation"),
        )?;
        execute_checked(
            executor,
            crate::targets::scp_upload(ssh, &local_config, &staging_config, false)
                .purpose("stage managed harness launch configuration"),
        )?;
        execute_checked(
            executor,
            crate::targets::ssh_command(ssh, ["chmod", "700", &staging_binary])
                .purpose("make managed harness preparation worker executable"),
        )?;
        execute_checked(
            executor,
            crate::targets::ssh_command(
                ssh,
                [
                    staging_binary.as_str(),
                    "worker",
                    "prepare-harness",
                    "--config",
                    staging_config.as_str(),
                ],
            )
            .purpose("prepare exact managed harness"),
        )?;
        Ok(())
    })();
    let cleanup = execute_checked(
        executor,
        crate::targets::ssh_command(ssh, ["rm", "-rf", "--", &staging_root])
            .purpose("remove managed harness preparation staging"),
    );
    match (result, cleanup) {
        (Ok(()), Ok(_)) => Ok(()),
        (Ok(()), Err(error)) => Err(error).context("clean managed harness preparation staging"),
        (Err(error), Ok(_)) => Err(error),
        (Err(error), Err(cleanup)) => {
            tracing::warn!(%cleanup, path = %staging_root, "managed harness preparation staging cleanup failed");
            Err(error)
        }
    }
}

pub(super) fn prepare_installed_managed_harness(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    worker_root: &str,
    launch: &WorkerLaunchConfig,
) -> Result<()> {
    if launch.harness_runtime != HarnessRuntimePolicy::Managed {
        return Ok(());
    }
    let worker_binary = format!("{worker_root}/hel");
    let launch_config = format!("{worker_root}/launch.json");
    let command = match locator {
        targets::TargetLocator::LocalBare { .. } => CommandSpec::new(
            worker_binary.clone(),
            [
                "worker",
                "prepare-harness",
                "--config",
                launch_config.as_str(),
            ],
        ),
        targets::TargetLocator::AwsEc2 { ssh, .. }
        | targets::TargetLocator::SshBare { ssh, .. } => crate::targets::ssh_command(
            ssh,
            [
                worker_binary.as_str(),
                "worker",
                "prepare-harness",
                "--config",
                launch_config.as_str(),
            ],
        ),
        _ => bail!("managed harness policy requires a local bare, SSH-bare, or EC2 target"),
    };
    execute_checked(
        executor,
        command.purpose("prepare exact managed harness before worker startup"),
    )?;
    Ok(())
}

pub(super) fn installed_worker_binary_replacement_plan(
    locator: &targets::TargetLocator,
    session_id: &str,
    worker_binary: &Path,
) -> Result<CommandPlan> {
    worker_binary_replacement_plan(locator, session_id, worker_binary, "hel")
}

fn worker_binary_replacement_plan(
    locator: &targets::TargetLocator,
    session_id: &str,
    worker_binary: &Path,
    installed_name: &str,
) -> Result<CommandPlan> {
    verify_worker_build(worker_binary)?;
    let worker_root = targets::worker_root(locator, session_id)?;
    let installed = format!("{worker_root}/{installed_name}");
    let staged = format!("{installed}.next");
    let commands = match locator {
        targets::TargetLocator::LocalBare { .. } => vec![
            CommandSpec::new(
                "cp",
                [worker_binary.to_string_lossy().into_owned(), staged.clone()],
            )
            .purpose("stage replacement Mjolnir worker"),
            CommandSpec::new("mv", ["-f", &staged, &installed])
                .purpose("replace installed Mjolnir worker"),
            CommandSpec::new("chmod", ["700", &installed])
                .purpose("make replaced Mjolnir worker executable"),
        ],
        targets::TargetLocator::LocalPodman { container_id, .. }
        | targets::TargetLocator::LocalDocker { container_id, .. }
        | targets::TargetLocator::AppleContainer { container_id, .. } => {
            let engine = match locator {
                targets::TargetLocator::LocalPodman { .. } => "podman",
                targets::TargetLocator::LocalDocker { .. } => "docker",
                targets::TargetLocator::AppleContainer { .. } => "container",
                _ => unreachable!("matched local container target"),
            };
            vec![
                CommandSpec::new(
                    engine,
                    [
                        "cp".into(),
                        worker_binary.to_string_lossy().into_owned(),
                        format!("{container_id}:{staged}"),
                    ],
                )
                .purpose("stage replacement Mjolnir worker"),
                CommandSpec::new(
                    engine,
                    container_upload_ownership_args(container_id, &worker_root, &[&staged]),
                )
                .purpose("assign replacement worker to the worker user"),
                CommandSpec::new(
                    engine,
                    [
                        "exec".into(),
                        container_id.clone(),
                        "mv".into(),
                        "-f".into(),
                        staged,
                        installed.clone(),
                    ],
                )
                .purpose("replace installed Mjolnir worker"),
                CommandSpec::new(
                    engine,
                    [
                        "exec".into(),
                        container_id.clone(),
                        "chmod".into(),
                        "700".into(),
                        installed,
                    ],
                )
                .purpose("make replaced Mjolnir worker executable"),
            ]
        }
        targets::TargetLocator::AwsEc2 { ssh, .. }
        | targets::TargetLocator::SshBare { ssh, .. } => vec![
            crate::targets::scp_upload(ssh, worker_binary, &staged, false)
                .purpose("stage replacement Mjolnir worker"),
            crate::targets::ssh_command(ssh, ["mv", "-f", "--", &staged, &installed])
                .purpose("replace installed Mjolnir worker"),
            crate::targets::ssh_command(ssh, ["chmod", "700", &installed])
                .purpose("make replaced Mjolnir worker executable"),
        ],
        targets::TargetLocator::SshPodman {
            ssh, container_id, ..
        }
        | targets::TargetLocator::SshDocker {
            ssh, container_id, ..
        } => {
            let engine = match locator {
                targets::TargetLocator::SshPodman { .. } => "podman",
                targets::TargetLocator::SshDocker { .. } => "docker",
                _ => unreachable!("matched remote container target"),
            };
            let upload = format!("{}/{session_id}-hel.next", targets::REMOTE_UPLOAD_STAGING);
            vec![
                crate::targets::ssh_command(ssh, ["mkdir", "-p", targets::REMOTE_UPLOAD_STAGING])
                    .purpose("create remote replacement worker staging"),
                crate::targets::scp_upload(ssh, worker_binary, &upload, false)
                    .purpose("stage replacement Mjolnir worker"),
                crate::targets::ssh_command(
                    ssh,
                    [engine, "cp", &upload, &format!("{container_id}:{staged}")],
                )
                .purpose("stage replacement Mjolnir worker"),
                crate::targets::ssh_command(
                    ssh,
                    std::iter::once(engine.to_owned()).chain(container_upload_ownership_args(
                        container_id,
                        &worker_root,
                        &[&staged],
                    )),
                )
                .purpose("assign replacement worker to the worker user"),
                crate::targets::ssh_command(
                    ssh,
                    [
                        engine,
                        "exec",
                        container_id,
                        "mv",
                        "-f",
                        "--",
                        &staged,
                        &installed,
                    ],
                )
                .purpose("replace installed Mjolnir worker"),
                crate::targets::ssh_command(
                    ssh,
                    [engine, "exec", container_id, "chmod", "700", &installed],
                )
                .purpose("make replaced Mjolnir worker executable"),
                crate::targets::ssh_command(ssh, ["rm", "-f", "--", &upload])
                    .purpose("remove remote replacement worker staging"),
            ]
        }
    };
    Ok(CommandPlan {
        description: format!("replace stale Mjolnir worker for session {session_id}"),
        commands,
    })
}

pub(super) fn installed_file_digest_command(
    locator: &targets::TargetLocator,
    path: &str,
    purpose: &str,
) -> CommandSpec {
    targets::locator_command(locator, vec!["sha256sum".into(), path.into()]).purpose(purpose)
}

pub(super) fn worker_launch_refresh_plan(
    locator: &targets::TargetLocator,
    session_id: &str,
    launch: &WorkerLaunchConfig,
) -> Result<WorkerLaunchRefreshPlan> {
    let worker_root = targets::worker_root(locator, session_id)?;
    let installed = format!("{worker_root}/launch.json");
    let staged = format!("{installed}.next");
    let staged_arg = targets::join_remote_command(std::slice::from_ref(&staged));
    let installed_arg = targets::join_remote_command(std::slice::from_ref(&installed));
    let script = format!("umask 077; cat > {staged_arg} && mv -f -- {staged_arg} {installed_arg}");
    let body = serde_json::to_vec_pretty(launch).context("serialize worker launch config")?;
    let expected_sha256 = lower_hex(Sha256::digest(&body));
    let replace = targets::locator_command(locator, vec!["sh".into(), "-c".into(), script])
        .purpose("replace stale Mjolnir worker launch config")
        .with_sensitive_stdin(body);
    Ok(WorkerLaunchRefreshPlan {
        expected_sha256,
        installed_digest: installed_file_digest_command(
            locator,
            &installed,
            "identify installed Mjolnir worker launch config",
        ),
        replace: CommandPlan {
            description: format!("replace stale Mjolnir launch config for session {session_id}"),
            commands: vec![replace],
        },
    })
}

/// Plan a refresh without resolving or hashing a worker binary. Both happen
/// only after recovery has proved that the worker needs a restart.
pub(super) fn worker_binary_refresh_plan(
    locator: &targets::TargetLocator,
    session_id: &str,
) -> Result<Option<WorkerBinaryRefresh>> {
    let worker_root = targets::worker_root(locator, session_id)?;
    let installed = format!("{worker_root}/hel");
    // Planning runs on controller event loops. Resolve and hash only in the
    // recovery task, for every target: even a local container can run a foreign
    // architecture, and an unavailable source must never silently disable the
    // binary refresh while installing a newer launch configuration.
    Ok(Some(WorkerBinaryRefresh::Deferred(
        DeferredWorkerBinaryRefresh {
            locator: locator.clone(),
            session_id: session_id.to_owned(),
            installed_digest: installed_file_digest_command(
                locator,
                &installed,
                "identify installed Mjolnir worker binary",
            ),
        },
    )))
}

/// Refresh a worker binary during recovery: pick the worker binary for
/// the target's own architecture, and copy it over the installed one only when
/// their digests differ. This runs inside the recovery task, where blocking
/// target I/O is allowed; it must never be called from a UI/event loop.
///
/// The digest gate is what stops a redeploy loop: once the right binary is
/// installed, its digest matches the source and nothing is copied again, even
/// though recovery may still restart the worker.
pub(crate) fn refresh_target_worker_binary_if_stale(
    executor: &impl CommandExecutor,
    refresh: &DeferredWorkerBinaryRefresh,
) -> Result<()> {
    let source = worker_binary_for(&refresh.locator, executor)
        .context("resolve the worker binary for the recovering target")?;
    replace_target_worker_binary_if_stale(
        executor,
        &refresh.locator,
        &refresh.session_id,
        &refresh.installed_digest,
        &source,
    )
    .map(|_| ())
}

/// Copy `source` over the installed worker only when the installed
/// digest differs from `source`'s. Returns whether a copy ran. Split from the
/// resolver above so the digest gate is testable without resolving a real
/// worker binary for a target architecture.
pub(super) fn replace_target_worker_binary_if_stale(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    session_id: &str,
    installed_digest: &CommandSpec,
    source: &Path,
) -> Result<bool> {
    // Digest equality is meaningful only after the source matches this build.
    verify_worker_build(source)?;
    let expected = mj_core::worker_launch::worker_executable_digest(source)?;
    let installed = executor
        .execute(installed_digest)
        .context("read the installed worker digest")?;
    let matches = installed.status == 0
        && String::from_utf8_lossy(&installed.stdout)
            .split_whitespace()
            .next()
            .is_some_and(|digest| digest.eq_ignore_ascii_case(&expected));
    if matches {
        return Ok(false);
    }
    installed_worker_binary_replacement_plan(locator, session_id, source)?
        .execute(executor)
        .context("replace stale relay worker binary")?;
    Ok(true)
}
