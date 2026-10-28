use super::*;
use crate::targets::CommandOutput;

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
        | targets::TargetLocator::SshDocker { .. }
        | targets::TargetLocator::SshAppleContainer { .. } => MJ_CONTAINER_ENVIRONMENT.to_owned(),
        targets::TargetLocator::AwsEc2 { workspace, .. } => format!(
            "## Mjolnir disposable environment\n\nThis session runs on a disposable Mjolnir EC2 instance. When the session closes, Mjolnir checkpoints everything in project workspace directories under `$HOME/{workspace}`, including committed work, staged and unstaged changes, and untracked files. Mjolnir then terminates the instance.\n\nEverything outside `$HOME/{workspace}`, including installed packages, the rest of `$HOME`, and `/tmp`, is ephemeral and will be lost. Keep durable results in the workspace or push them to a remote.\n\nNew workspaces start on their own session branch from the default network fetch remote’s default branch. Local unpublished commits and uncommitted files are not copied. Use normal git push to publish the current branch to the configured network push destination. Closing saves a checkpoint; it does not publish commits or update the original local checkout. Resumed sessions restore their saved work.\n"
        ),
        targets::TargetLocator::LocalBare { .. } | targets::TargetLocator::SshBare { .. } => {
            return Ok(());
        }
    };
    append_staged_instructions(harness, destination, &environment)
}

pub(super) fn append_container_storage_guidance(
    harness: HarnessKind,
    destination: &Path,
    target: &targets::TargetLocator,
    workspace: Option<&Path>,
    native_temporary_storage: bool,
) -> Result<()> {
    let podman_workspace = match target {
        targets::TargetLocator::LocalPodman {
            workspace_storage, ..
        }
        | targets::TargetLocator::SshPodman {
            workspace_storage, ..
        } => Some(workspace_storage),
        targets::TargetLocator::LocalDocker { .. } | targets::TargetLocator::SshDocker { .. } => {
            None
        }
        _ => return Ok(()),
    };
    let heading = "## Mjolnir container storage\n\n";
    let mut guidance = String::from(heading);
    if native_temporary_storage {
        guidance.push_str("`/tmp` is a private disk-backed volume outside the container's writable overlay. Use it for disposable temporary files. Its contents are excluded from checkpoints and are lost when the session stops or restarts.\n\n");
    }
    if podman_workspace
        .is_some_and(|storage| !matches!(storage, targets::PodmanWorkspaceLocator::ContainerLayer))
    {
        let workspace = targets::container_workspace_root(workspace);
        guidance.push_str(&format!(
            "The workspace storage is mounted at `{workspace}`. "
        ));
        if workspace != targets::CONTAINER_WORKSPACE {
            guidance.push_str(
                "The parent `/workspace` directory itself is on the container's writable overlay. ",
            );
        }
        guidance.push_str("Keep durable results inside the project repository directories; arbitrary files elsewhere on the workspace volume are not checkpointed.\n");
    }
    if guidance == heading {
        return Ok(());
    }
    append_staged_instructions(harness, destination, &guidance)
}

pub(super) fn append_subagent_policy(
    harness: mj_core::config::HarnessKind,
    destination: &Path,
    policy: &mj_core::subagent::SubagentPolicy,
    limit: usize,
    offers_subtle: bool,
) -> Result<()> {
    if matches!(policy, mj_core::subagent::SubagentPolicy::Mjolnir { .. }) {
        append_staged_instructions(
            harness,
            destination,
            &mj_core::subagent::delegation_policy(limit, offers_subtle),
        )?;
    }
    Ok(())
}

pub(super) fn append_staged_instructions(
    harness: mj_core::config::HarnessKind,
    destination: &Path,
    text: &str,
) -> Result<()> {
    let path = destination.join(harness.agent_instructions_file());
    let separator = match std::fs::read_to_string(&path) {
        Ok(contents) if contents.contains(text) => return Ok(()),
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
    file.write_all(text.as_bytes())?;
    Ok(())
}

/// A profile's own login file that a worker on this machine links to instead
/// of copying (see [`HarnessKind::shares_local_login`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::controller) struct SharedLogin {
    /// Where the login file sits in a staged home.
    pub(in crate::controller) name: &'static str,
    /// The profile's login file the staged home links to.
    pub(in crate::controller) file: PathBuf,
}

/// The login file a worker for `profile` on `backend` shares with the profile,
/// or `None` when the worker keeps its own copy: on every other target, for a
/// harness that cannot share one, and for a profile that signs in some other
/// way than the harness's login file.
pub(in crate::controller) fn shared_profile_login(
    profile: &HarnessProfile,
    backend: &targets::TargetLocator,
) -> Option<SharedLogin> {
    if !(mj_core::targets::HOST_RUNS_WORKERS
        && matches!(backend, targets::TargetLocator::LocalBare { .. })
        && profile.kind.shares_local_login()
        && profile.auth_scheme().uses_native_login_file())
    {
        return None;
    }
    // A relative link would resolve against the worker's home, not the
    // directory the profile home is relative to; without an absolute path the
    // worker keeps a copy.
    let file = std::path::absolute(profile.authentication_marker()).ok()?;
    Some(SharedLogin {
        name: profile.kind.credential_file_name(),
        file,
    })
}

/// Copy a staged home into a home on this machine, then link the login file
/// when the worker shares the profile's own.
///
/// A link an earlier install left is removed before copying, whether or not
/// this install shares the login: copying a file through it would write the
/// stage over the profile's login.
pub(super) fn install_local_profile_stage(
    stage: &Path,
    home: &Path,
    shared_login: Option<&SharedLogin>,
) -> Result<()> {
    for entry in std::fs::read_dir(stage)? {
        let entry = entry?;
        let destination = home.join(entry.file_name());
        remove_link(&destination)?;
        copy_profile_entry(&entry.path(), &destination)?;
    }
    if let Some(login) = shared_login {
        link_shared_login(home, login)?;
    }
    Ok(())
}

/// Replace the login file in `home` with a link to the profile's own.
pub(in crate::controller) fn link_shared_login(home: &Path, login: &SharedLogin) -> Result<()> {
    let link = home.join(login.name);
    match std::fs::remove_file(&link) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            return Err(anyhow::Error::new(error)
                .context(format!("remove staged login copy {}", link.display())));
        }
    }
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(&login.file, &link);
    // Only Unix machines run local workers (`HOST_RUNS_WORKERS`).
    #[cfg(not(unix))]
    let linked = Err(std::io::Error::new(
        ErrorKind::Unsupported,
        "linking a staged login needs a Unix file system",
    ));
    linked.with_context(|| {
        format!(
            "link {} to the profile login {}",
            link.display(),
            login.file.display()
        )
    })
}

fn remove_link(path: &Path) -> Result<()> {
    if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        std::fs::remove_file(path)
            .with_context(|| format!("remove the earlier link {}", path.display()))?;
    }
    Ok(())
}

pub(super) fn copy_profile_entry(source: &Path, destination: &Path) -> Result<()> {
    copy_profile_entry_within(source, destination, &HashSet::new(), &[])
}

/// [`copy_profile_entry`], leaving out the paths in `excluded`. Each is
/// compared with the path the walk reaches it by, below `source` as given,
/// not with where a link resolves.
pub(super) fn copy_profile_entry_except(
    source: &Path,
    destination: &Path,
    excluded: &[PathBuf],
) -> Result<()> {
    copy_profile_entry_within(source, destination, &HashSet::new(), excluded)
}

/// Copy one profile entry, following symlinks so a profile home that links its
/// settings or instructions elsewhere still stages their contents. `entered`
/// holds the canonical paths of the directories already entered on this branch
/// of the recursion, which stops a symlinked directory cycle.
pub(super) fn copy_profile_entry_within(
    source: &Path,
    destination: &Path,
    entered: &HashSet<PathBuf>,
    excluded: &[PathBuf],
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
        entries
            .par_iter()
            .filter(|entry| !excluded.contains(&entry.path()))
            .try_for_each(|entry| {
                copy_profile_entry_within(
                    &entry.path(),
                    &destination.join(entry.file_name()),
                    &entered,
                    excluded,
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

// Container copies may be root-owned while the worker directory belongs to a
// different user. Run this ownership-only operation as container root; normal
// Podman execs inherit the user recorded when each container was created.
pub(in crate::controller) fn container_upload_ownership_args(
    engine: &str,
    container_id: &str,
    worker_root: &str,
    paths: &[&str],
) -> Vec<String> {
    let root_user = if engine == "podman" { "0:0" } else { "0" };
    let mut args = vec![
        "exec".into(),
        "--user".into(),
        root_user.into(),
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

/// Link every mbx lookup to the one policy already reachable through the
/// shared cache mount. File symlinks follow atomic replacements at the source.
const MBX_CONFIG_SCRIPT: &str = r#"set -eu
source=$1
shift
[ -r "$source/config.toml" ] || { echo 'shared machine mbx configuration is missing' >&2; exit 1; }
for root in "$HOME/.config" "$@"; do
    mkdir -p -- "$root/mbx"
    link="$root/mbx/.config.mj-$$"
    ln -s -- "$source/config.toml" "$link"
    mv -Tf -- "$link" "$root/mbx/config.toml"
done
"#;

const CHECK_LEGACY_MBX_AND_LINK_CONFIG_SCRIPT: &str = r#"set -eu
bin=$1 source=$2
shift 2
scheme=new
if [ -f "$bin/mbx" ]; then
    if grep -Fqx '# mjolnir-mbx-shim' "$bin/mbx"; then
        scheme=new
    else
        grep_status=$?
        case "$grep_status" in
            1) scheme=legacy ;;
            *)
                echo "could not inspect the Mjolnir mbx shim marker in $bin/mbx (grep exited $grep_status)" >&2
                exit 1
                ;;
        esac
    fi
fi
if [ "$scheme" = legacy ]; then
    [ -r "$source/config.toml" ] || { echo 'shared machine mbx configuration is missing' >&2; exit 1; }
    for root in "$HOME/.config" "$@"; do
        mkdir -p -- "$root/mbx"
        link="$root/mbx/.config.mj-$$"
        ln -s -- "$source/config.toml" "$link"
        mv -Tf -- "$link" "$root/mbx/config.toml"
    done
    printf 'legacy'
else
    printf 'new'
fi
"#;

const INSTALL_MBX_SHIMS_SCRIPT: &str = r#"set -eu
bin=$1 cargo_body=$2 mbx_body=$3
mkdir -p -- "$bin"
write_shim() {
    target=$1 body=$2
    temporary=$(mktemp "${target}.mj-XXXXXX")
    trap 'rm -f -- "$temporary"' EXIT HUP INT TERM
    {
        printf '%s\n' '#!/bin/sh' '# mjolnir-mbx-shim'
        printf '%s\n' "$body"
    } >"$temporary"
    chmod 755 -- "$temporary"
    mv -f -- "$temporary" "$target"
    trap - EXIT HUP INT TERM
}
write_shim "$bin/cargo" "$cargo_body"
write_shim "$bin/mbx" "$mbx_body"
"#;

const REMOVE_MBX_SHIMS_SCRIPT: &str = r#"set -eu
bin=$1
for name in cargo mbx; do
    path="$bin/$name"
    if [ -f "$path" ] && [ ! -L "$path" ] && grep -Fqx '# mjolnir-mbx-shim' "$path"; then
        rm -f -- "$path"
    fi
done
"#;

/// Install marked Cargo and mbx launchers that use the shared cache copy.
pub(super) fn install_mbx_shims(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    worker_root: &str,
    mbx_binary: &Path,
    configuration: &Path,
    config_roots: &[PathBuf],
) -> Result<()> {
    ensure!(
        mj_core::target_path::is_absolute_on_host_or_target(mbx_binary),
        "shared cache mbx path is not absolute: {}",
        mbx_binary.display()
    );
    let bin = format!("{worker_root}/bin");
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
        | targets::TargetLocator::SshAppleContainer { .. }
        | targets::TargetLocator::AwsEc2 { .. }
        | targets::TargetLocator::SshBare { .. } => {
            bail!("the build cache is only installed into Podman and Docker containers")
        }
    };
    let quoted_binary = targets::posix_quote(&mbx_binary.to_string_lossy());
    let cargo_body = format!(
        "MBX_CARGO_SHIM_MODE=1\nMBX_CARGO_SHIM_PATH=$(command -v \"$0\")\nexport MBX_CARGO_SHIM_MODE MBX_CARGO_SHIM_PATH\nexec {quoted_binary} \"$@\""
    );
    let mbx_body = format!("exec {quoted_binary} \"$@\"");
    let args = vec![
        engine.to_owned(),
        "exec".to_owned(),
        container_id.clone(),
        "sh".to_owned(),
        "-c".to_owned(),
        INSTALL_MBX_SHIMS_SCRIPT.to_owned(),
        "sh".to_owned(),
        bin,
        cargo_body,
        mbx_body,
    ];
    let command = match ssh {
        None => CommandSpec::new(args[0].clone(), args[1..].iter().cloned()),
        Some(ssh) => crate::targets::ssh_command(ssh, args),
    }
    .purpose("install the shared-cache mbx shims")
    .stage(ProvisionStage::Syncing);
    execute_checked(executor, command)?;
    link_mbx_configuration(executor, locator, configuration, config_roots)
}

pub(super) fn remove_generated_mbx_shims(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    worker_root: &str,
) -> Result<()> {
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
        | targets::TargetLocator::SshAppleContainer { .. }
        | targets::TargetLocator::AwsEc2 { .. }
        | targets::TargetLocator::SshBare { .. } => {
            bail!("the build cache is only installed into Podman and Docker containers")
        }
    };
    let args = vec![
        engine.to_owned(),
        "exec".to_owned(),
        container_id.clone(),
        "sh".to_owned(),
        "-c".to_owned(),
        REMOVE_MBX_SHIMS_SCRIPT.to_owned(),
        "sh".to_owned(),
        format!("{worker_root}/bin"),
    ];
    let container_command = match ssh {
        None => CommandSpec::new(args[0].clone(), args[1..].iter().cloned()),
        Some(ssh) => crate::targets::ssh_command(ssh, args),
    }
    .purpose("remove marked mbx shims after cache verification failed")
    .stage(ProvisionStage::Syncing);
    execute_checked(executor, container_command)?;
    Ok(())
}

pub(super) fn verify_mbx_binary(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    mbx_binary: &Path,
    expected_version: &str,
) -> Result<()> {
    ensure!(
        mj_core::target_path::is_absolute_on_host_or_target(mbx_binary),
        "shared cache mbx path is not absolute: {}",
        mbx_binary.display()
    );
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
        | targets::TargetLocator::SshAppleContainer { .. }
        | targets::TargetLocator::AwsEc2 { .. }
        | targets::TargetLocator::SshBare { .. } => {
            bail!("the build cache is only installed into Podman and Docker containers")
        }
    };
    let args = vec![
        engine.to_owned(),
        "exec".to_owned(),
        container_id.clone(),
        mbx_binary.to_string_lossy().into_owned(),
        "--version".to_owned(),
    ];
    let command = match ssh {
        None => CommandSpec::new(args[0].clone(), args[1..].iter().cloned()),
        Some(ssh) => crate::targets::ssh_command(ssh, args),
    }
    .purpose("verify the synchronized mbx inside the session container")
    .stage(ProvisionStage::Syncing);
    let output = executor.execute(&command)?;
    let container_version = reported_mbx_version(output, &command)?;
    ensure!(
        expected_version == container_version,
        "the container reports mbx {container_version}, but the synchronized host copy is mbx {expected_version}"
    );
    Ok(())
}

fn reported_mbx_version(output: CommandOutput, command: &CommandSpec) -> Result<String> {
    ensure!(
        output.status == 0,
        "{} failed with status {}: {}",
        command.purpose,
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next_back()
        .map(str::to_owned)
        .context("mbx --version returned no version")
}

/// Upgrade configuration without touching an executable a live build may use.
pub(super) fn link_mbx_configuration(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    configuration: &Path,
    config_roots: &[PathBuf],
) -> Result<()> {
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
        | targets::TargetLocator::SshAppleContainer { .. }
        | targets::TargetLocator::AwsEc2 { .. }
        | targets::TargetLocator::SshBare { .. } => {
            bail!("the build cache is only installed into Podman and Docker containers")
        }
    };
    {
        let mut args = vec![
            engine.to_owned(),
            "exec".into(),
            "-i".into(),
            container_id.clone(),
            "sh".into(),
            "-c".into(),
            MBX_CONFIG_SCRIPT.to_owned(),
            "mj-mbx-config".into(),
            configuration.to_string_lossy().into_owned(),
        ];
        args.extend(
            config_roots
                .iter()
                .map(|path| path.to_string_lossy().into_owned()),
        );
        let command = match ssh {
            None => CommandSpec::new(args[0].clone(), args[1..].iter().cloned()),
            Some(ssh) => crate::targets::ssh_command(ssh, args),
        }
        .purpose("link the shared machine mbx configuration")
        .stage(ProvisionStage::Syncing);
        execute_checked(executor, command)?;
    }
    Ok(())
}

/// Preserve legacy private mbx installations and link their shared config in
/// the same container command that distinguishes them from generated shims.
pub(super) fn link_legacy_mbx_configuration(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    worker_root: &str,
    configuration: &Path,
    config_roots: &[PathBuf],
) -> Result<bool> {
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
        | targets::TargetLocator::SshAppleContainer { .. }
        | targets::TargetLocator::AwsEc2 { .. }
        | targets::TargetLocator::SshBare { .. } => {
            bail!("the build cache is only installed into Podman and Docker containers")
        }
    };
    let mut args = vec![
        engine.to_owned(),
        "exec".to_owned(),
        container_id.clone(),
        "sh".to_owned(),
        "-c".to_owned(),
        CHECK_LEGACY_MBX_AND_LINK_CONFIG_SCRIPT.to_owned(),
        "sh".to_owned(),
        format!("{worker_root}/bin"),
        configuration.to_string_lossy().into_owned(),
    ];
    args.extend(
        config_roots
            .iter()
            .map(|path| path.to_string_lossy().into_owned()),
    );
    let container_command = match ssh {
        None => CommandSpec::new(args[0].clone(), args[1..].iter().cloned()),
        Some(ssh) => crate::targets::ssh_command(ssh, args),
    }
    .purpose("inspect legacy mbx and relink its shared configuration")
    .stage(ProvisionStage::Syncing);
    let output = execute_checked(executor, container_command)?;
    match output.stdout.as_slice() {
        b"legacy" => Ok(true),
        b"new" => Ok(false),
        other => bail!(
            "container mbx scheme inspection returned unexpected output: {}",
            String::from_utf8_lossy(other)
        ),
    }
}

pub(super) struct StagedWorkerProfile<'a> {
    pub launch: &'a Path,
    pub ownership: &'a Path,
    pub profile: &'a Path,
    pub shared_login: Option<&'a SharedLogin>,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn install_worker_files(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    session_id: &str,
    worker_root: &str,
    profile_home: &str,
    worker: &WorkerInstall,
    launch_config: &Path,
    ownership: &Path,
    profile_stage: &Path,
    shared_login: Option<&SharedLogin>,
) -> Result<()> {
    if let Some(binary) = worker.host_binary() {
        verify_worker_build(binary)?;
    }
    install_worker_profile_files(
        executor,
        locator,
        session_id,
        worker_root,
        profile_home,
        StagedWorkerProfile {
            launch: launch_config,
            ownership,
            profile: profile_stage,
            shared_login,
        },
    )?;
    install_worker_binary(executor, locator, session_id, worker, "hel")
}

pub(super) fn install_worker_profile_files(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    session_id: &str,
    worker_root: &str,
    profile_home: &str,
    staged: StagedWorkerProfile<'_>,
) -> Result<()> {
    let StagedWorkerProfile {
        launch: launch_config,
        ownership,
        profile: profile_stage,
        shared_login,
    } = staged;
    match locator {
        targets::TargetLocator::LocalBare { .. } => {
            if profile_stage.is_dir() {
                // A staged home that is a link to a profile home, left for a
                // session an earlier release started there, is replaced by a
                // directory of its own. Copying through it would write this
                // stage over the person's own configuration.
                if std::fs::symlink_metadata(profile_home)
                    .is_ok_and(|metadata| metadata.file_type().is_symlink())
                {
                    std::fs::remove_file(profile_home).with_context(|| {
                        format!("unlink the earlier session's profile home link {profile_home}")
                    })?;
                }
                std::fs::create_dir_all(profile_home).context("create isolated local profile")?;
                install_local_profile_stage(profile_stage, Path::new(profile_home), shared_login)?;
            }
            for command in [
                CommandSpec::new("mkdir", ["-p", worker_root])
                    .purpose("create local bare worker directory"),
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
            let launch_installed = format!("{worker_root}/launch.json");
            let ownership_installed = format!("{worker_root}/ownership.json");
            let owned = [
                launch_installed.as_str(),
                ownership_installed.as_str(),
                profile_home,
            ];
            let mut commands = vec![
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
                        launch_config.to_string_lossy().into_owned(),
                        format!("{container_id}:{launch_installed}"),
                    ],
                )
                .purpose("upload worker launch configuration"),
                CommandSpec::new(
                    engine,
                    [
                        "cp".into(),
                        ownership.to_string_lossy().into_owned(),
                        format!("{container_id}:{ownership_installed}"),
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
                    container_upload_ownership_args(engine, container_id, worker_root, &owned),
                )
                .purpose("match uploaded files to the worker directory owner"),
            ];
            commands.push(
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
            );
            for command in commands {
                execute_checked(executor, command)?;
            }
        }
        targets::TargetLocator::AwsEc2 { ssh, .. } => {
            install_worker_over_ssh(
                executor,
                ssh,
                worker_root,
                profile_home,
                launch_config,
                ownership,
                profile_stage,
            )?;
        }
        targets::TargetLocator::SshBare { ssh, .. } => {
            install_worker_over_ssh(
                executor,
                ssh,
                worker_root,
                profile_home,
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
        }
        | targets::TargetLocator::SshAppleContainer {
            ssh, container_id, ..
        } => {
            let engine = locator
                .container_engine()
                .expect("matched a remote container target");
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
            let launch_installed = format!("{worker_root}/launch.json");
            let ownership_installed = format!("{worker_root}/ownership.json");
            let owned = [
                launch_installed.as_str(),
                ownership_installed.as_str(),
                profile_home,
            ];
            let mut remote = vec![
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
                    format!("{upload}/launch.json"),
                    format!("{container_id}:{launch_installed}"),
                ],
                vec![
                    engine.into(),
                    "cp".into(),
                    format!("{upload}/ownership.json"),
                    format!("{container_id}:{ownership_installed}"),
                ],
                // Apple's `container cp` copies a relative `dir/.` as `dir`
                // itself, nesting the profile one level too deep; only an
                // absolute source copies the contents, as Docker and Podman
                // do for either form. The staging path is home-relative.
                vec![
                    "sh".into(),
                    "-c".into(),
                    r#"exec "$0" cp "$HOME/$1/." "$2""#.into(),
                    engine.into(),
                    format!("{upload}/profile"),
                    format!("{container_id}:{profile_home}"),
                ],
                std::iter::once(engine.to_owned())
                    .chain(container_upload_ownership_args(
                        engine,
                        container_id,
                        worker_root,
                        &owned,
                    ))
                    .collect(),
            ];
            remote.extend([
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
            ]);
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

/// Refresh configuration without removing native history or skills the harness
/// maintains itself. Absent destination settings must not inherit the source's.
pub(super) fn clear_profile_configuration(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    session_id: &str,
    home: &str,
    harness: HarnessKind,
) -> Result<()> {
    let mut paths = profile_configuration_paths(harness);
    paths.push(harness.credential_file_name());
    paths.extend([
        mj_core::aws_credentials::SHARED_CREDENTIALS_FILE,
        mj_core::aws_credentials::CONFIG_FILE,
        mj_core::aws_credentials::EMPTY_CREDENTIALS_FILE,
    ]);
    match harness {
        HarnessKind::Codex => paths.push(STAGED_CATALOG_FILE),
        HarnessKind::Mimo => paths.push("config"),
        _ => {}
    }
    paths.sort_unstable();
    paths.dedup();
    let mut script = String::from("set -eu\n");
    for relative in paths {
        let path = mj_core::target_path::join(Path::new(home), Path::new(relative));
        let quoted = targets::posix_quote(&mj_core::target_path::text(&path));
        let protected = harness
            .harness_owned_skill_paths()
            .iter()
            .filter_map(|owned| {
                Path::new(owned)
                    .strip_prefix(relative)
                    .ok()
                    .and_then(|tail| tail.components().next())
                    .map(|name| targets::posix_quote(&name.as_os_str().to_string_lossy()))
            })
            .collect::<Vec<_>>();
        if protected.is_empty() {
            script.push_str(&format!("rm -rf -- {quoted}\n"));
        } else {
            script.push_str(&format!(
                "if [ -L {quoted} ]; then rm -f -- {quoted}; else\n\
                 for entry in {quoted}/* {quoted}/.[!.]* {quoted}/..?*; do\n\
                 case \"${{entry##*/}}\" in {}) continue ;; esac\n\
                 rm -rf -- \"$entry\"\ndone\nfi\n",
                protected.join("|"),
            ));
        }
    }
    execute_checked(
        executor,
        targets::command_on_locator(
            locator,
            session_id,
            vec!["sh".into(), "-c".into(), script],
            "replace profile configuration while retaining native state",
        )?,
    )?;
    Ok(())
}

/// Install the session-private SSH metadata; binary installation is shared
/// with creation, upgrades and recovery in `install_worker_binary`.
#[allow(clippy::too_many_arguments)]
pub(super) fn install_worker_over_ssh(
    executor: &impl CommandExecutor,
    ssh: &SshTarget,
    worker_root: &str,
    profile_home: &str,
    launch_config: &Path,
    ownership: &Path,
    profile_stage: &Path,
) -> Result<()> {
    execute_checked(
        executor,
        crate::targets::ssh_command(ssh, ["mkdir", "-p", worker_root, profile_home])
            .purpose("create SSH worker directories"),
    )?;
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
        crate::targets::ssh_command(ssh, ["chmod", "-R", "go-rwx", profile_home])
            .purpose("restrict SSH harness profile permissions"),
    )?;
    Ok(())
}

#[cfg(test)]
mod mbx_shim_tests {
    use super::*;

    #[cfg(target_os = "linux")]
    fn execute_script(
        script: &str,
        arguments: impl IntoIterator<Item = String>,
        path: Option<&Path>,
        home: Option<&Path>,
    ) -> CommandOutput {
        let args = std::iter::once("-c".to_owned())
            .chain(std::iter::once(script.to_owned()))
            .chain(std::iter::once("sh".to_owned()))
            .chain(arguments)
            .collect::<Vec<_>>();
        let mut command = CommandSpec::new("sh", args);
        if let Some(path) = path {
            command
                .env
                .insert("PATH".into(), format!("{}:/usr/bin:/bin", path.display()));
        }
        if let Some(home) = home {
            command
                .env
                .insert("HOME".into(), home.to_string_lossy().into_owned());
        }
        targets::ProcessExecutor.execute(&command).unwrap()
    }

    #[cfg(target_os = "linux")]
    fn run_script(script: &str, arguments: impl IntoIterator<Item = String>) {
        let output = execute_script(script, arguments, None, None);
        assert_eq!(
            output.status,
            0,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn only_marked_mbx_shims_are_removed() {
        let root = tempfile::tempdir().unwrap();
        let generated = root.path().join("generated");
        run_script(
            INSTALL_MBX_SHIMS_SCRIPT,
            [
                generated.to_string_lossy().into_owned(),
                "MBX_CARGO_SHIM_MODE=1\nMBX_CARGO_SHIM_PATH=$(command -v \"$0\")\nexport MBX_CARGO_SHIM_MODE MBX_CARGO_SHIM_PATH\nexec /cache/.mjolnir/bin/mbx \"$@\"".into(),
                "exec /cache/.mjolnir/bin/mbx \"$@\"".into(),
            ],
        );
        for name in ["cargo", "mbx"] {
            let script = std::fs::read_to_string(generated.join(name)).unwrap();
            assert!(script.lines().any(|line| line == "# mjolnir-mbx-shim"));
        }

        let legacy = root.path().join("legacy");
        std::fs::create_dir(&legacy).unwrap();
        std::fs::write(legacy.join("cargo"), b"legacy cargo binary").unwrap();
        std::fs::write(legacy.join("mbx"), b"legacy copied mbx binary").unwrap();

        run_script(
            REMOVE_MBX_SHIMS_SCRIPT,
            [generated.to_string_lossy().into_owned()],
        );
        run_script(
            REMOVE_MBX_SHIMS_SCRIPT,
            [legacy.to_string_lossy().into_owned()],
        );

        assert!(!generated.join("cargo").exists());
        assert!(!generated.join("mbx").exists());
        assert_eq!(
            std::fs::read(legacy.join("cargo")).unwrap(),
            b"legacy cargo binary"
        );
        assert_eq!(
            std::fs::read(legacy.join("mbx")).unwrap(),
            b"legacy copied mbx binary"
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn legacy_detection_distinguishes_grep_miss_match_and_failure() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("shared config");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("config.toml"), "[cache]\n").unwrap();
        let roots = temp.path().join("config roots");
        let home = temp.path().join("home");
        let legacy = temp.path().join("legacy");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(legacy.join("mbx"), b"private copied mbx binary").unwrap();
        let args = || {
            vec![
                legacy.to_string_lossy().into_owned(),
                source.to_string_lossy().into_owned(),
                roots.to_string_lossy().into_owned(),
            ]
        };

        let legacy_output = execute_script(
            CHECK_LEGACY_MBX_AND_LINK_CONFIG_SCRIPT,
            args(),
            None,
            Some(&home),
        );
        assert_eq!(legacy_output.status, 0);
        assert_eq!(legacy_output.stdout, b"legacy");

        std::fs::write(legacy.join("mbx"), "#!/bin/sh\n# mjolnir-mbx-shim\n").unwrap();
        let new_output = execute_script(
            CHECK_LEGACY_MBX_AND_LINK_CONFIG_SCRIPT,
            args(),
            None,
            Some(&home),
        );
        assert_eq!(new_output.status, 0);
        assert_eq!(new_output.stdout, b"new");

        let tools = temp.path().join("tools");
        std::fs::create_dir_all(&tools).unwrap();
        std::fs::write(tools.join("grep"), "#!/bin/sh\nexit 2\n").unwrap();
        std::fs::set_permissions(tools.join("grep"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::fs::write(legacy.join("mbx"), b"private copied mbx binary").unwrap();
        let failed = execute_script(
            CHECK_LEGACY_MBX_AND_LINK_CONFIG_SCRIPT,
            args(),
            Some(&tools),
            Some(&home),
        );
        assert_ne!(failed.status, 0);
        assert!(
            String::from_utf8_lossy(&failed.stderr).contains("grep exited 2"),
            "{}",
            String::from_utf8_lossy(&failed.stderr)
        );
    }

    #[test]
    fn legacy_detection_errors_are_returned_to_the_caller() {
        struct FailedInspection;
        impl crate::targets::CommandExecutor for FailedInspection {
            fn execute(&self, _command: &CommandSpec) -> anyhow::Result<CommandOutput> {
                Ok(CommandOutput {
                    status: 1,
                    stdout: Vec::new(),
                    stderr: b"could not inspect the Mjolnir mbx shim marker in /worker/bin/mbx (grep exited 2)".to_vec(),
                })
            }
        }

        let error = link_legacy_mbx_configuration(
            &FailedInspection,
            &targets::TargetLocator::LocalPodman {
                borrowed_from: None,
                container_id: "mj-test-container".into(),
                workspace_storage: targets::PodmanWorkspaceLocator::ContainerLayer,
            },
            "/worker",
            Path::new("/cache/.mjolnir/config/mbx"),
            &[],
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("grep exited 2"), "{message}");
    }
}
