use super::*;

/// Add lifecycle guidance only for targets that Hel destroys as a whole.
pub(super) fn append_hel_target_environment(
    harness: mj_core::config::HarnessKind,
    destination: &Path,
    target: &targets::TargetLocator,
) -> Result<()> {
    let environment = match target {
        targets::TargetLocator::LocalPodman { .. }
        | targets::TargetLocator::LocalDocker { .. }
        | targets::TargetLocator::AppleContainer { .. }
        | targets::TargetLocator::SshPodman { .. }
        | targets::TargetLocator::SshDocker { .. } => MJ_CONTAINER_ENVIRONMENT.to_owned(),
        targets::TargetLocator::AwsEc2 { workspace, .. } => format!(
            "## Mjolnir disposable environment\n\nThis session runs on a disposable Mjolnir EC2 instance. When the session closes, Mjolnir checkpoints everything in project workspace directories under `$HOME/{workspace}`, including committed work, staged and unstaged changes, and untracked files. Mjolnir then terminates the instance.\n\nEverything outside `$HOME/{workspace}`, including installed packages, the rest of `$HOME`, and `/tmp`, is ephemeral and will be lost. Keep durable results in the workspace or push them to a remote.\n\nNew workspaces start on their own session branch from the default network fetch remote’s default branch. Local unpublished commits and uncommitted files are not copied. Use normal git push to publish the current branch to the configured network push destination. Closing saves a checkpoint; it does not publish commits or update the original local checkout. Resumed sessions restore their saved work.\n"
        ),
        targets::TargetLocator::LocalBare { .. } | targets::TargetLocator::SshBare { .. } => {
            return Ok(());
        }
    };
    let path = destination.join(harness.agent_instructions_file());
    let separator = match std::fs::read_to_string(&path) {
        Ok(contents) if !contents.is_empty() && !contents.ends_with('\n') => "\n\n",
        Ok(contents) if !contents.is_empty() => "\n",
        Ok(_) => "",
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "",
        Err(error) => return Err(error.into()),
    };
    use std::io::Write;

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open staged harness instructions {}", path.display()))?;
    file.write_all(separator.as_bytes())?;
    file.write_all(environment.as_bytes())?;
    Ok(())
}

pub(super) fn copy_profile_entry(source: &Path, destination: &Path) -> Result<()> {
    copy_profile_entry_within(source, destination, &HashSet::new())
}

/// Copy one profile entry, following symlinks so a profile home that links its
/// settings or instructions elsewhere still stages their contents. `entered`
/// holds the canonical paths of the directories already entered on this branch
/// of the recursion, which stops a symlinked directory cycle.
pub(super) fn copy_profile_entry_within(
    source: &Path,
    destination: &Path,
    entered: &HashSet<PathBuf>,
) -> Result<()> {
    std::fs::symlink_metadata(source)
        .with_context(|| format!("read staged profile entry metadata {}", source.display()))?;
    let metadata = match std::fs::metadata(source) {
        Ok(metadata) => metadata,
        // The entry exists but its link target does not; staging the rest of
        // the profile is more useful than failing on a stale link.
        Err(error) if error.kind() == ErrorKind::NotFound => {
            tracing::warn!(
                source = %source.display(),
                "skipping staged profile entry whose symlink target is missing"
            );
            return Ok(());
        }
        Err(error) => {
            return Err(anyhow::Error::new(error).context(format!(
                "read staged profile entry metadata {}",
                source.display()
            )));
        }
    };
    if metadata.is_file() {
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create staged profile directory {}", parent.display()))?;
        }
        std::fs::copy(source, destination).with_context(|| {
            format!(
                "copy staged profile file {} to {}",
                source.display(),
                destination.display()
            )
        })?;
        return Ok(());
    }
    if metadata.is_dir() {
        let canonical = std::fs::canonicalize(source)
            .with_context(|| format!("resolve staged profile directory {}", source.display()))?;
        if entered.contains(&canonical) {
            tracing::warn!(
                source = %source.display(),
                target = %canonical.display(),
                "skipping staged profile directory that links back into itself"
            );
            return Ok(());
        }
        let mut entered = entered.clone();
        entered.insert(canonical);
        std::fs::create_dir_all(destination).with_context(|| {
            format!("create staged profile directory {}", destination.display())
        })?;
        let entries = std::fs::read_dir(source)
            .with_context(|| format!("list staged profile directory {}", source.display()))?
            .collect::<std::io::Result<Vec<_>>>()
            .with_context(|| {
                format!(
                    "read staged profile directory entries in {}",
                    source.display()
                )
            })?;
        // Sibling entries in one directory are independent, so recurse in
        // parallel; this is the level most likely to hold many files (e.g. a
        // skills or plugins tree).
        entries.par_iter().try_for_each(|entry| {
            copy_profile_entry_within(
                &entry.path(),
                &destination.join(entry.file_name()),
                &entered,
            )
        })?;
        std::fs::set_permissions(destination, metadata.permissions()).with_context(|| {
            format!(
                "set permissions for staged profile directory {}",
                destination.display()
            )
        })?;
    }
    Ok(())
}

// Container copies can create root-owned files even when exec defaults to a
// non-root image user. The worker directory was created by that user, so use
// its ownership for uploaded files before restricting their permissions.
pub(in crate::controller) fn container_upload_ownership_args(
    container_id: &str,
    worker_root: &str,
    paths: &[&str],
) -> Vec<String> {
    let mut args = vec![
        "exec".into(),
        "--user".into(),
        "0".into(),
        container_id.into(),
        "sh".into(),
        "-c".into(),
        // GNU and BusyBox stat both support this numeric ownership format.
        r#"set -eu; owner=$(stat -c '%u:%g' -- "$1"); shift; chown -R "$owner" -- "$@""#.into(),
        "sh".into(),
        worker_root.into(),
    ];
    args.extend(paths.iter().map(|path| (*path).to_owned()));
    args
}

/// Shell that writes the host's mbx configuration into the container user's
/// own home, where the container's mbx reads it.
const MBX_CONFIG_SCRIPT: &str =
    r#"set -eu; mkdir -p "$HOME/.config/mbx"; cat > "$HOME/.config/mbx/config.toml""#;

/// Install `bin/mbx` and its `bin/cargo` shim beside the worker, and hand the
/// container the host's mbx configuration when the host has one.
///
/// `bin` is the directory the worker later writes its `gh` wrapper into and
/// prepends to `PATH`; it creates that directory without clearing it, so these
/// two files survive and the shim is found before the image's own Cargo.
pub(super) fn install_mbx_files(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    session_id: &str,
    worker_root: &str,
    binary: &Path,
    configuration: Option<&str>,
) -> Result<()> {
    let bin = format!("{worker_root}/bin");
    let mbx = format!("{bin}/mbx");
    let cargo = format!("{bin}/cargo");
    // A hard link keeps one copy of a 30 MB binary; a copy is the fallback for
    // images whose layer cannot link.
    let shim_script = format!(r#"ln -f "{mbx}" "{cargo}" 2>/dev/null || cp -f "{mbx}" "{cargo}""#);
    let (engine, container_id, ssh) = match locator {
        targets::TargetLocator::LocalPodman { container_id, .. } => ("podman", container_id, None),
        targets::TargetLocator::LocalDocker { container_id, .. } => ("docker", container_id, None),
        targets::TargetLocator::SshPodman {
            ssh, container_id, ..
        } => ("podman", container_id, Some(ssh)),
        targets::TargetLocator::SshDocker {
            ssh, container_id, ..
        } => ("docker", container_id, Some(ssh)),
        targets::TargetLocator::LocalBare { .. }
        | targets::TargetLocator::AppleContainer { .. }
        | targets::TargetLocator::AwsEc2 { .. }
        | targets::TargetLocator::SshBare { .. } => {
            bail!("the build cache is only installed into Podman and Docker containers")
        }
    };
    // Remote hosts keep the binary in a content-addressed cache so it crosses
    // the network once per unique mbx, exactly as the worker binary does.
    let source = match ssh {
        None => binary.to_string_lossy().into_owned(),
        Some(ssh) => {
            let digest = mj_core::worker_launch::worker_executable_digest(binary)?;
            let cache_dir = format!(".cache/mjolnir/mbx/{digest}");
            let cached = format!("{cache_dir}/mbx");
            let present = matches!(
                executor.execute(
                    &crate::targets::ssh_command(ssh, ["test", "-f", &cached])
                        .purpose("probe the cached remote mbx binary"),
                ),
                Ok(output) if output.status == 0
            );
            if !present {
                execute_checked(
                    executor,
                    crate::targets::ssh_command(ssh, ["mkdir", "-p", &cache_dir])
                        .purpose("create the remote mbx cache"),
                )?;
                let partial = format!("{cache_dir}/mbx.partial-{session_id}");
                execute_checked(
                    executor,
                    crate::targets::scp_upload(ssh, binary, &partial, false)
                        .purpose("upload the remote mbx binary"),
                )?;
                execute_checked(
                    executor,
                    crate::targets::ssh_command(ssh, ["mv", &partial, &cached])
                        .purpose("publish the cached remote mbx binary"),
                )?;
            }
            cached
        }
    };
    let steps: Vec<(Vec<String>, &str)> = vec![
        (
            vec![
                engine.into(),
                "exec".into(),
                container_id.clone(),
                "mkdir".into(),
                "-p".into(),
                bin.clone(),
            ],
            "create the session binary directory",
        ),
        (
            vec![
                engine.into(),
                "cp".into(),
                source,
                format!("{container_id}:{mbx}"),
            ],
            "upload the mbx build cache binary",
        ),
        (
            std::iter::once(engine.to_owned())
                .chain(container_upload_ownership_args(
                    container_id,
                    worker_root,
                    &[&bin],
                ))
                .collect(),
            "assign the mbx binary to the worker user",
        ),
        (
            vec![
                engine.into(),
                "exec".into(),
                container_id.clone(),
                "sh".into(),
                "-c".into(),
                shim_script,
            ],
            "install the mbx Cargo shim",
        ),
        (
            vec![
                engine.into(),
                "exec".into(),
                container_id.clone(),
                "chmod".into(),
                "755".into(),
                mbx.clone(),
                cargo.clone(),
            ],
            "make the mbx build cache executable",
        ),
    ];
    for (args, purpose) in steps {
        let command = match ssh {
            None => CommandSpec::new(args[0].clone(), args[1..].iter().cloned()),
            Some(ssh) => crate::targets::ssh_command(ssh, args),
        }
        .purpose(purpose)
        .stage(ProvisionStage::Syncing);
        execute_checked(executor, command)?;
    }
    if let Some(configuration) = configuration {
        let args = vec![
            engine.to_owned(),
            "exec".into(),
            "-i".into(),
            container_id.clone(),
            "sh".into(),
            "-c".into(),
            MBX_CONFIG_SCRIPT.to_owned(),
        ];
        let command = match ssh {
            None => CommandSpec::new(args[0].clone(), args[1..].iter().cloned()),
            Some(ssh) => crate::targets::ssh_command(ssh, args),
        }
        .purpose("install the host mbx configuration")
        .stage(ProvisionStage::Syncing)
        .with_sensitive_stdin(configuration.as_bytes().to_vec());
        execute_checked(executor, command)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn install_worker_files(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    session_id: &str,
    worker_root: &str,
    profile_home: &str,
    worker_binary: &Path,
    launch_config: &Path,
    ownership: &Path,
    profile_stage: &Path,
) -> Result<()> {
    verify_worker_build(worker_binary)?;
    match locator {
        targets::TargetLocator::LocalBare { .. } => {
            if profile_stage.is_dir() {
                std::fs::create_dir_all(profile_home).context("create isolated local profile")?;
                for entry in std::fs::read_dir(profile_stage)? {
                    let entry = entry?;
                    copy_profile_entry(
                        &entry.path(),
                        &Path::new(profile_home).join(entry.file_name()),
                    )?;
                }
            }
            for command in [
                CommandSpec::new("mkdir", ["-p", worker_root])
                    .purpose("create local bare worker directory"),
                CommandSpec::new(
                    "cp",
                    [
                        worker_binary.to_string_lossy().into_owned(),
                        format!("{worker_root}/hel"),
                    ],
                )
                .purpose("install local Mjolnir worker"),
                CommandSpec::new(
                    "cp",
                    [
                        launch_config.to_string_lossy().into_owned(),
                        format!("{worker_root}/launch.json"),
                    ],
                )
                .purpose("install local worker launch configuration"),
                CommandSpec::new(
                    "cp",
                    [
                        ownership.to_string_lossy().into_owned(),
                        format!("{worker_root}/ownership.json"),
                    ],
                )
                .purpose("install local worker ownership marker"),
                CommandSpec::new("chmod", ["700", &format!("{worker_root}/hel")])
                    .purpose("make local Mjolnir worker executable"),
            ] {
                execute_checked(executor, command)?;
            }
        }
        targets::TargetLocator::LocalPodman { container_id, .. }
        | targets::TargetLocator::LocalDocker { container_id, .. }
        | targets::TargetLocator::AppleContainer { container_id, .. } => {
            let engine = match locator {
                targets::TargetLocator::LocalPodman { .. } => "podman",
                targets::TargetLocator::LocalDocker { .. } => "docker",
                targets::TargetLocator::AppleContainer { .. } => "container",
                _ => unreachable!("matched local container target"),
            };
            for command in [
                CommandSpec::new(
                    engine,
                    [
                        "exec".into(),
                        container_id.clone(),
                        "mkdir".into(),
                        "-p".into(),
                        worker_root.into(),
                        profile_home.into(),
                    ],
                )
                .purpose("create target worker directories"),
                CommandSpec::new(
                    engine,
                    [
                        "cp".into(),
                        worker_binary.to_string_lossy().into_owned(),
                        format!("{container_id}:{worker_root}/hel"),
                    ],
                )
                .purpose("upload Mjolnir worker"),
                CommandSpec::new(
                    engine,
                    [
                        "cp".into(),
                        launch_config.to_string_lossy().into_owned(),
                        format!("{container_id}:{worker_root}/launch.json"),
                    ],
                )
                .purpose("upload worker launch configuration"),
                CommandSpec::new(
                    engine,
                    [
                        "cp".into(),
                        ownership.to_string_lossy().into_owned(),
                        format!("{container_id}:{worker_root}/ownership.json"),
                    ],
                )
                .purpose("upload worker ownership marker"),
                CommandSpec::new(
                    engine,
                    [
                        "cp".into(),
                        format!("{}/.", profile_stage.display()),
                        format!("{container_id}:{profile_home}"),
                    ],
                )
                .purpose("upload harness profile allowlist"),
                CommandSpec::new(
                    engine,
                    container_upload_ownership_args(
                        container_id,
                        worker_root,
                        &[
                            &format!("{worker_root}/hel"),
                            &format!("{worker_root}/launch.json"),
                            &format!("{worker_root}/ownership.json"),
                            profile_home,
                        ],
                    ),
                )
                .purpose("assign uploaded files to the worker user"),
                CommandSpec::new(
                    engine,
                    [
                        "exec".into(),
                        container_id.clone(),
                        "chmod".into(),
                        "700".into(),
                        format!("{worker_root}/hel"),
                    ],
                )
                .purpose("make Mjolnir worker executable"),
                CommandSpec::new(
                    engine,
                    [
                        "exec".into(),
                        container_id.clone(),
                        "chmod".into(),
                        "-R".into(),
                        "go-rwx".into(),
                        profile_home.into(),
                    ],
                )
                .purpose("restrict harness profile permissions"),
            ] {
                execute_checked(executor, command)?;
            }
        }
        // A disposable EC2 instance hosts one session and is terminated with
        // it, so a cache there would only hold a second copy of the worker.
        targets::TargetLocator::AwsEc2 { ssh, .. } => {
            install_worker_over_ssh(
                executor,
                ssh,
                None,
                worker_root,
                profile_home,
                worker_binary,
                launch_config,
                ownership,
                profile_stage,
            )?;
        }
        targets::TargetLocator::SshBare { ssh, .. } => {
            let cached_worker = cache_worker_on_ssh_host(executor, ssh, session_id, worker_binary)?;
            install_worker_over_ssh(
                executor,
                ssh,
                Some(&cached_worker),
                worker_root,
                profile_home,
                worker_binary,
                launch_config,
                ownership,
                profile_stage,
            )?;
        }
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
            let cached_worker = cache_worker_on_ssh_host(executor, ssh, session_id, worker_binary)?;
            let upload = format!("{}/{session_id}", targets::REMOTE_UPLOAD_STAGING);
            execute_checked(
                executor,
                crate::targets::ssh_command(ssh, ["mkdir", "-p", &upload])
                    .purpose("create remote upload staging"),
            )?;
            for (source, name) in [
                (launch_config, "launch.json"),
                (ownership, "ownership.json"),
            ] {
                execute_checked(
                    executor,
                    crate::targets::scp_upload(ssh, source, &format!("{upload}/{name}"), false)
                        .purpose("upload remote container worker file"),
                )?;
            }
            execute_checked(
                executor,
                crate::targets::scp_upload(ssh, profile_stage, &format!("{upload}/profile"), true)
                    .purpose("upload remote container profile allowlist"),
            )?;
            let remote = [
                vec![
                    engine.into(),
                    "exec".into(),
                    container_id.clone(),
                    "mkdir".into(),
                    "-p".into(),
                    worker_root.into(),
                    profile_home.into(),
                ],
                vec![
                    engine.into(),
                    "cp".into(),
                    cached_worker.clone(),
                    format!("{container_id}:{worker_root}/hel"),
                ],
                vec![
                    engine.into(),
                    "cp".into(),
                    format!("{upload}/launch.json"),
                    format!("{container_id}:{worker_root}/launch.json"),
                ],
                vec![
                    engine.into(),
                    "cp".into(),
                    format!("{upload}/ownership.json"),
                    format!("{container_id}:{worker_root}/ownership.json"),
                ],
                vec![
                    engine.into(),
                    "cp".into(),
                    format!("{upload}/profile/."),
                    format!("{container_id}:{profile_home}"),
                ],
                std::iter::once(engine.to_owned())
                    .chain(container_upload_ownership_args(
                        container_id,
                        worker_root,
                        &[
                            &format!("{worker_root}/hel"),
                            &format!("{worker_root}/launch.json"),
                            &format!("{worker_root}/ownership.json"),
                            profile_home,
                        ],
                    ))
                    .collect(),
                vec![
                    engine.into(),
                    "exec".into(),
                    container_id.clone(),
                    "chmod".into(),
                    "700".into(),
                    format!("{worker_root}/hel"),
                ],
                vec![
                    engine.into(),
                    "exec".into(),
                    container_id.clone(),
                    "chmod".into(),
                    "-R".into(),
                    "go-rwx".into(),
                    profile_home.into(),
                ],
                vec!["rm".into(), "-rf".into(), "--".into(), upload.clone()],
            ];
            for args in remote {
                execute_checked(
                    executor,
                    crate::targets::ssh_command(ssh, args)
                        .purpose("install remote container worker"),
                )?;
            }
        }
    }
    Ok(())
}

/// Put the worker binary in the SSH host's content-addressed cache and answer
/// with its path there. The binary crosses the network only when the host
/// does not hold this build yet.
///
/// A worker binary is well over 100 MB and identical across sessions. Uploading
/// it once per session made each create on a slow link hold a daemon action
/// slot for minutes (R3-4). Every session on the host, SSH-bare or in a remote
/// container, copies its own worker from here instead.
///
/// Two sessions that find the cache empty at the same time each upload to
/// their own partial name and rename it into place, so the cached path only
/// ever names a complete file.
fn cache_worker_on_ssh_host(
    executor: &impl CommandExecutor,
    ssh: &SshTarget,
    session_id: &str,
    worker_binary: &Path,
) -> Result<String> {
    let digest = mj_core::worker_launch::worker_executable_digest(worker_binary)?;
    // Home-relative, not "~/": targets::ssh_command single-quotes every
    // argument, so a tilde would stay literal in the remote shell while scp
    // expands it, and the two sides would disagree. Both ssh commands (cwd is
    // the login home) and scp resolve a relative path against the remote home.
    let cache_dir = format!(".cache/mjolnir/workers/{digest}");
    let cached_worker = format!("{cache_dir}/hel");
    let cached = matches!(
        executor.execute(
            &crate::targets::ssh_command(ssh, ["test", "-f", &cached_worker])
                .purpose("probe cached remote Mjolnir worker"),
        ),
        Ok(output) if output.status == 0
    );
    if cached {
        return Ok(cached_worker);
    }
    execute_checked(
        executor,
        crate::targets::ssh_command(ssh, ["mkdir", "-p", &cache_dir])
            .purpose("create remote worker cache"),
    )?;
    let partial = format!("{cache_dir}/hel.partial-{session_id}");
    if let Err(error) = execute_checked(
        executor,
        crate::targets::scp_upload(ssh, worker_binary, &partial, false)
            .purpose("upload Mjolnir worker to the remote cache"),
    ) {
        // The partial file is outside every session's worker root, so no
        // session cleanup would ever remove it. Best effort: a connection
        // that failed the upload may fail this too.
        let _ = executor.execute(
            &crate::targets::ssh_command(ssh, ["rm", "-f", "--", &partial])
                .purpose("remove partial remote worker upload"),
        );
        return Err(error);
    }
    execute_checked(
        executor,
        crate::targets::ssh_command(ssh, ["mv", &partial, &cached_worker])
            .purpose("publish cached remote Mjolnir worker"),
    )?;
    Ok(cached_worker)
}

/// Install a worker on a host reached over SSH with no container.
///
/// `cached_worker` is the host's cached copy of the worker binary, from
/// [`cache_worker_on_ssh_host`]; the session gets its own copy of it, so its
/// worker root and cleanup are the same as with an upload. Without one, the
/// local binary is uploaded straight into the worker root.
#[allow(clippy::too_many_arguments)]
pub(super) fn install_worker_over_ssh(
    executor: &impl CommandExecutor,
    ssh: &SshTarget,
    cached_worker: Option<&str>,
    worker_root: &str,
    profile_home: &str,
    worker_binary: &Path,
    launch_config: &Path,
    ownership: &Path,
    profile_stage: &Path,
) -> Result<()> {
    execute_checked(
        executor,
        crate::targets::ssh_command(ssh, ["mkdir", "-p", worker_root, profile_home])
            .purpose("create SSH worker directories"),
    )?;
    let worker = format!("{worker_root}/hel");
    let install_worker = match cached_worker {
        Some(cached_worker) => crate::targets::ssh_command(ssh, ["cp", cached_worker, &worker])
            .purpose("copy cached Mjolnir worker into the session"),
        None => crate::targets::scp_upload(ssh, worker_binary, &worker, false)
            .purpose("upload SSH worker file"),
    };
    execute_checked(executor, install_worker)?;
    for (source, remote) in [
        (launch_config, format!("{worker_root}/launch.json")),
        (ownership, format!("{worker_root}/ownership.json")),
    ] {
        execute_checked(
            executor,
            crate::targets::scp_upload(ssh, source, &remote, false)
                .purpose("upload SSH worker file"),
        )?;
    }
    let incoming_profile = format!("{profile_home}.incoming");
    execute_checked(
        executor,
        crate::targets::scp_upload(ssh, profile_stage, &incoming_profile, true)
            .purpose("upload SSH harness profile allowlist"),
    )?;
    execute_checked(
        executor,
        crate::targets::ssh_command(
            ssh,
            ["cp", "-R", &format!("{incoming_profile}/."), profile_home],
        )
        .purpose("install SSH harness profile allowlist"),
    )?;
    execute_checked(
        executor,
        crate::targets::ssh_command(ssh, ["rm", "-rf", "--", &incoming_profile])
            .purpose("remove SSH profile staging"),
    )?;
    execute_checked(
        executor,
        crate::targets::ssh_command(ssh, ["chmod", "700", &format!("{worker_root}/hel")])
            .purpose("make SSH worker executable"),
    )?;
    execute_checked(
        executor,
        crate::targets::ssh_command(ssh, ["chmod", "-R", "go-rwx", profile_home])
            .purpose("restrict SSH harness profile permissions"),
    )?;
    Ok(())
}
