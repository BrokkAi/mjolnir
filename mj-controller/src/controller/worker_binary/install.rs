use super::*;
use crate::targets::CommandOutput;
use mj_core::targets::SshTrafficClass;

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
        | targets::TargetLocator::SshAppleContainer { .. }
        | targets::TargetLocator::SshNitro { .. } => MJ_CONTAINER_ENVIRONMENT.to_owned(),
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
    attachments: &[targets::AdditionalMount],
) -> Result<()> {
    // Whether the workspace sits on storage of its own that the guidance
    // should name: a Podman volume or helper path, or a Nitro sandbox's volume.
    let workspace_mounted = match target {
        targets::TargetLocator::LocalPodman {
            workspace_storage, ..
        }
        | targets::TargetLocator::SshPodman {
            workspace_storage, ..
        } => !matches!(
            workspace_storage,
            targets::PodmanWorkspaceLocator::ContainerLayer
        ),
        targets::TargetLocator::SshNitro { .. } => true,
        targets::TargetLocator::LocalDocker { .. }
        | targets::TargetLocator::SshDocker { .. }
        | targets::TargetLocator::AppleContainer { .. }
        | targets::TargetLocator::SshAppleContainer { .. } => false,
        _ => return Ok(()),
    };
    let heading = "## Mjolnir container storage\n\n";
    let mut guidance = String::from(heading);
    if native_temporary_storage {
        guidance.push_str("`/tmp` is a private disk-backed volume outside the container's writable overlay. Use it for disposable temporary files. Its contents are excluded from checkpoints and are lost when the session stops or restarts.\n\n");
    }
    if workspace_mounted {
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
    if guidance != heading {
        append_staged_instructions(harness, destination, &guidance)?;
    }
    if attachments.is_empty() {
        return Ok(());
    }
    let mut guidance = String::from(
        "## Mjolnir external attachments\n\nThese host directories are attached at the container paths listed below. External attachments are not included in project workspace checkpoints.\n\n",
    );
    for attachment in attachments {
        let behavior = match attachment.access {
            targets::MountAccess::Ro => {
                "read-only; you can read these files but cannot change them"
            }
            targets::MountAccess::Cow => {
                "copy-on-write; changes are private to this container, never change the host directory, and are lost when the container is removed or recreated"
            }
            targets::MountAccess::Rw => {
                "read-write; changes write directly to the host directory and persist after the container is removed"
            }
        };
        guidance.push_str(&format!(
            "- Container `{}` from host `{}`: `{}` ({behavior}).\n",
            attachment.destination.display(),
            attachment.source.display(),
            attachment.access.label(),
        ));
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
    // The microVM engine runs every exec as the session's configured user,
    // which is root (see the run arguments), so it needs no user flag.
    let user_flag: Vec<String> = match engine {
        "podman" => vec!["--user".into(), "0:0".into()],
        targets::MICROVM_ENGINE => Vec::new(),
        _ => vec!["--user".into(), "0".into()],
    };
    let mut args = vec!["exec".into()];
    args.extend(user_flag);
    args.extend([
        container_id.into(),
        "sh".into(),
        "-c".into(),
        // GNU and BusyBox stat both support this numeric ownership format.
        r#"set -eu; owner=$(stat -c '%u:%g' -- "$1"); shift; chown -R "$owner" -- "$@""#.into(),
        "sh".into(),
        worker_root.into(),
    ]);
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
        | targets::TargetLocator::SshNitro { .. }
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
    let steps = [
        targets::CommandStep::new("install the shared-cache mbx shims", args),
        targets::CommandStep::new(
            "link the shared machine mbx configuration",
            mbx_configuration_argv(engine, container_id, configuration, config_roots),
        ),
    ];
    let args = targets::step_script(steps);
    let command = match ssh {
        None => CommandSpec::new(args[0].clone(), args[1..].iter().cloned()),
        Some(ssh) => crate::targets::ssh_command(SshTrafficClass::Control, ssh, args),
    }
    .purpose("install the shared-cache mbx shims")
    .stage(ProvisionStage::Syncing);
    execute_checked(executor, command)?;
    Ok(())
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
        | targets::TargetLocator::SshNitro { .. }
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
        Some(ssh) => crate::targets::ssh_command(SshTrafficClass::Control, ssh, args),
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
        | targets::TargetLocator::SshNitro { .. }
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
        Some(ssh) => crate::targets::ssh_command(SshTrafficClass::Control, ssh, args),
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
fn mbx_configuration_argv(
    engine: &str,
    container_id: &str,
    configuration: &Path,
    config_roots: &[PathBuf],
) -> Vec<String> {
    let mut args = vec![
        engine.to_owned(),
        "exec".into(),
        "-i".into(),
        container_id.to_owned(),
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
    args
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
        | targets::TargetLocator::SshNitro { .. }
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
        Some(ssh) => crate::targets::ssh_command(SshTrafficClass::Control, ssh, args),
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
    pub shared_paths: &'a [&'a str],
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
    shared_paths: &[&str],
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
            shared_paths,
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
        shared_paths,
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
                shared_paths,
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
                shared_paths,
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
        }
        | targets::TargetLocator::SshNitro {
            ssh, container_id, ..
        } => {
            let engine = locator
                .container_engine()
                .expect("matched a remote container target");
            let upload = format!("{}/{session_id}", targets::REMOTE_UPLOAD_STAGING);
            execute_checked(
                executor,
                crate::targets::ssh_command(
                    SshTrafficClass::Control,
                    ssh,
                    ["mkdir", "-p", &upload],
                )
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
            upload_profile_archive(
                executor,
                ssh,
                profile_stage,
                &format!("{upload}/profile"),
                shared_paths,
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
                    format!(
                        r#"exec {} cp "$HOME/$1/." "$2""#,
                        targets::engine_shell_words(engine)
                    ),
                    "mj-profile-copy".into(),
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
            // Staging removal stays a final success-only step, as before.
            let steps = remote.into_iter().map(|args| {
                targets::CommandStep::new(
                    "install remote container worker",
                    targets::with_engine_argv(args),
                )
            });
            execute_checked(
                executor,
                crate::targets::ssh_command_owned(
                    SshTrafficClass::Bulk,
                    ssh,
                    targets::step_script(steps),
                )
                .purpose("install remote container worker"),
            )?;
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
    paths.extend(mj_core::aws_credentials::FILES);
    if harness == HarnessKind::Codex {
        paths.push(STAGED_CATALOG_FILE);
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
            SshTrafficClass::Control,
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
    shared_paths: &[&str],
) -> Result<()> {
    execute_checked(
        executor,
        crate::targets::ssh_command(
            SshTrafficClass::Control,
            ssh,
            ["mkdir", "-p", worker_root, profile_home],
        )
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
    upload_profile_archive(
        executor,
        ssh,
        profile_stage,
        &incoming_profile,
        shared_paths,
    )?;
    execute_checked(
        executor,
        crate::targets::ssh_command(
            SshTrafficClass::Bulk,
            ssh,
            ["cp", "-R", &format!("{incoming_profile}/."), profile_home],
        )
        .purpose("install SSH harness profile allowlist"),
    )?;
    execute_checked(
        executor,
        crate::targets::ssh_command(
            SshTrafficClass::Control,
            ssh,
            ["rm", "-rf", "--", &incoming_profile],
        )
        .purpose("remove SSH profile staging"),
    )?;
    execute_checked(
        executor,
        crate::targets::ssh_command(
            SshTrafficClass::Control,
            ssh,
            ["chmod", "-R", "go-rwx", profile_home],
        )
        .purpose("restrict SSH harness profile permissions"),
    )?;
    Ok(())
}

const PROFILE_CONTENT_STORE: &str = ".cache/mjolnir/profile-content";
const PROFILE_CONTENT_MISSING: i32 = 75;
// A retired/incomplete stage consumes the same existing retry slot as a
// missing content pin, but needs a fresh host query rather than missing keys.
const PROFILE_STAGE_MISSING: i32 = 76;

// Registration and collection take the same store gate. A collector's
// snapshot cannot predate a directory that an active attempt is about to use.
struct ProfileUpload {
    store: String,
    token: String,
}

type ProfileUploads = std::sync::Mutex<HashMap<String, HashSet<String>>>;

fn profile_uploads() -> &'static ProfileUploads {
    static UPLOADS: OnceLock<ProfileUploads> = OnceLock::new();
    UPLOADS.get_or_init(Default::default)
}

impl ProfileUpload {
    fn register(store: &str, token: &str) -> Self {
        let mut uploads = profile_uploads().lock().unwrap_or_else(|e| e.into_inner());
        uploads
            .entry(store.to_owned())
            .or_default()
            .insert(token.to_owned());
        Self {
            store: store.to_owned(),
            token: token.to_owned(),
        }
    }

    fn active(store: &str) -> Vec<String> {
        profile_uploads()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(store)
            .map_or_else(Vec::new, |tokens| tokens.iter().cloned().collect())
    }
}

impl Drop for ProfileUpload {
    fn drop(&mut self) {
        let mut uploads = profile_uploads().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(tokens) = uploads.get_mut(&self.store) {
            tokens.remove(&self.token);
            if tokens.is_empty() {
                uploads.remove(&self.store);
            }
        }
    }
}

// Reuse teardown's process identity, including boot/start identity on Linux.
// A failed identity query never authorizes deletion.
fn profile_script(body: &str) -> String {
    format!(
        "{}\nmj_profile_stage_missing_status={PROFILE_STAGE_MISSING}\n{PROFILE_OWNER_SCRIPT}\n{body}",
        mj_core::subprocess::PROCESS_BIRTH_SCRIPT
    )
}

const PROFILE_OWNER_SCRIPT: &str = r#"mj_profile_stage_missing() {
    echo "stage removed by a collector or incomplete: $work" >&2
    return "$mj_profile_stage_missing_status"
}
mj_profile_check_marker() {
    marker=$(cat -- "$1" 2>/dev/null) || { mj_profile_stage_missing; return "$mj_profile_stage_missing_status"; }
    [ "$marker" = "${work##*/}" ] || mj_profile_stage_missing
}
mj_profile_claim() {
    birth=$(mj_process_birth "$$") || birth=
    if [ -z "$birth" ]; then
        echo "cannot identify profile stage process $$ in $work" >&2
        return 1
    fi
    # The first write of a later phase replaces its owner atomically. Never
    # recreate a collected directory, or treat a partial receive as complete.
    if ! { printf '%s\n%s\n' "$$" "$birth" > "$work/.owner.next" &&
           mv -- "$work/.owner.next" "$work/.owner"; }; then
        mj_profile_stage_missing; return "$mj_profile_stage_missing_status"
    fi
    mj_profile_check_marker "$work/.token"
}
mj_profile_quiescent() {
    # Foreground payload work has finished. Match the exact owning identity,
    # so a killed later phase cannot inherit its predecessor's completion.
    if ! { cat -- "$work/.owner" > "$work/.quiescent.next" &&
           mv -- "$work/.quiescent.next" "$work/.quiescent"; }; then
        mj_profile_stage_missing; return "$mj_profile_stage_missing_status"
    fi
}
mj_profile_warn_unknown() {
    recorded=$(tr '\n' ':' < "$1/.owner" 2>/dev/null) || recorded=unavailable
    echo "profile staging ownership unknown or work not quiescent; retaining $1 (owner $recorded)" >&2
}
mj_profile_owner_state() {
    pid=$(sed -n '1p' "$1/.owner" 2>/dev/null) || pid=
    birth=$(sed -n '2p' "$1/.owner" 2>/dev/null) || birth=
    case $pid in *[!0123456789]*|'') printf unknown; return ;; esac
    [ -n "$birth" ] || { printf unknown; return; }
    current=$(mj_process_birth "$pid" 2>/dev/null) || current=
    if [ -n "$current" ]; then
        if [ "$current" = "$birth" ]; then printf alive; return; fi
    else
        # ps's documented empty selection is status 1 without output/errors.
        # An unreadable birth alone does not prove absence.
        status=0
        selected=$(LC_ALL=C ps -o pid= -p "$pid" 2> "$2") || status=$?
        if [ "$status" != 1 ] || [ -n "$selected" ] || [ -s "$2" ]; then
            printf unknown; return
        fi
    fi
    # A dead shell alone does not prove its tar/cp children stopped. Only
    # that exact owner can attest that all foreground work finished.
    if cmp -s "$1/.owner" "$1/.quiescent"; then printf dead; else printf unknown; fi
}"#;

// Match store.rs:COLLECT and COLLECT_INTERVAL: retain content for a day and
// collect at most hourly. Touch requested content before collecting it.
const QUERY_PROFILE_CONTENT: &str = r#"set -eu
umask 077
store="$HOME/$1"; shift
mkdir -p -- "$store"
chmod -- 700 "$store"
present=$(mktemp "$store/.present-XXXXXXXXXX")
trap 'rm -f -- "$present"' EXIT
while IFS= read -r key; do
    if [ -f "$store/$key" ]; then
        printf '%s\n' "$key" >> "$present"
    else
        printf '%s\n' "$key"
    fi
done
(cd "$store" && xargs sh -c '[ "$#" -eq 0 ] || touch -- "$@"' sh) < "$present"
if [ ! -f "$store/.collected" ] || [ -n "$(find "$store/.collected" -mmin +60 -print)" ]; then
    find "$store" -mindepth 1 -maxdepth 1 -type f -mmin +1440 -exec rm -f -- {} +
    # Upload and pin directories that an interrupted run left behind.
    find "$store" -mindepth 1 -maxdepth 1 -type d ! -name '.stage-*' ! -name '.retired-*' -mmin +1440 -exec rm -rf -- {} +
    touch -- "$store/.collected"
fi
# Private staging is reclaimed by ownership, never by age. The caller holds
# the store gate and passes its live registry from that same locked view.
mj_profile_restore() {
    if [ -e "$2" ] || ! mv -- "$1" "$2"; then
        echo "could not restore profile staging; retaining $1" >&2
    fi
}
for work in "$store"/.stage-* "$store"/.retired-*; do
    [ -d "$work" ] || continue
    name=$(cat -- "$work/.token" 2>/dev/null) || name=
    case $name in .stage-*) ;; *)
        mj_profile_warn_unknown "$work"; continue ;;
    esac
    case $name in *[!abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-]*)
        mj_profile_warn_unknown "$work"; continue ;;
    esac
    token=${name#.stage-}
    active=0
    for live in "$@"; do [ "$live" != "$token" ] || active=1; done
    [ "$active" = 0 ] || continue
    snapshot=$(cat -- "$work/.owner" 2>/dev/null) || snapshot=
    state=$(mj_profile_owner_state "$work" "$present")
    case ${work##*/} in .retired-*)
        case $state in
            dead) rm -rf -- "$work" ;;
            alive) mj_profile_restore "$work" "$store/$name" ;;
            *) mj_profile_warn_unknown "$work" ;;
        esac
        continue ;;
    esac
    if [ "$state" = alive ]; then continue; fi
    if [ "$state" != dead ] || [ "${work##*/}" != "$name" ]; then
        mj_profile_warn_unknown "$work"; continue
    fi
    # Claim and retirement contend on the original directory path. Once it is
    # retired no later phase can install an owner marker or begin using it.
    retired=$(mktemp -d "$store/.retired-XXXXXXXXXX")
    rmdir -- "$retired"
    mv -- "$work" "$retired" 2>/dev/null || continue
    after=$(cat -- "$retired/.owner" 2>/dev/null) || after=
    state=$(mj_profile_owner_state "$retired" "$present")
    if [ "$after" != "$snapshot" ] || [ "$state" = alive ]; then
        mj_profile_restore "$retired" "$work"
    elif [ "$state" = dead ]; then
        rm -rf -- "$retired"
    else
        mj_profile_warn_unknown "$retired"
    fi
done"#;

// The archive holds the profile under `profile/`, with private files and a
// relative symlink per shared path into `.mj-store`; beside it, outside the
// profile's names, `.mj-content/<key>` for content the host lacked and
// `.mj-keys` (every shared key, one per line). This script extracts it into a
// private work directory and points `.mj-store` there at the store. One dereferencing `cp -RL` then materializes the
// whole profile, so the cost does not grow with one process per file.
const STAGE_PROFILE_CONTENT: &str = r#"set -eu
umask 077
work="$HOME/$1"
mkdir -p -- "$work"
trap 'rm -rf -- "$work"' EXIT
printf '%s\n' "${work##*/}" > "$work/.token"
mj_profile_claim
tar -xzf - --no-same-owner --no-same-permissions -C "$work"
printf '%s\n' "${work##*/}" > "$work/.received"
mj_profile_quiescent
trap - EXIT"#;

const INSTALL_PROFILE_CONTENT: &str = r#"set -eu
umask 077
destination=$1
store="$HOME/$2"
work="$HOME/$3"
mj_profile_claim
mj_profile_check_marker "$work/.received"
[ -d "$work/profile" ] && [ -f "$work/.mj-keys" ] || { mj_profile_stage_missing; exit "$mj_profile_stage_missing_status"; }
rm -rf -- "$destination"
mkdir -p -- "$(dirname -- "$destination")"
chmod -- 700 "$store"
upload=
pins=
trap 'rm -rf -- "$work" ${upload:+"$upload"} ${pins:+"$pins"}' EXIT
tree="$work/profile"
content="$work/.mj-content"
if [ -d "$content" ]; then
    case $(uname -s) in Darwin) sum='shasum -a 256';; *) sum=sha256sum;; esac
    expected=$(cd "$content" && find . -type f | wc -l)
    upload="$work/.upload"
    mkdir -- "$upload"
    find "$content" -type f -exec sh -c 'cp -- "$@" "$0"' "$upload" {} +
    # Verify the copies that will be published, in one digest pass: a
    # truncated copy fails its digest and a missing one fails the count.
    # Relative names (./<hex key>) keep the store path out of the output,
    # where sha256sum would escape a backslash or newline in it.
    (cd "$upload" && find . -type f -exec $sum -- {} +) > "$work/sums"
    verified=$(awk '{ key = $0; sub(/.*\//, "", key); digest = key; sub(/\.x$/, "", digest);
        if ($1 != digest) { print "profile content digest mismatch: " key > "/dev/stderr"; bad = 1 } n++ }
        END { if (bad) exit 1; print n + 0 }' "$work/sums")
    [ "$verified" -eq "$expected" ] || { echo "verified $verified of $expected profile content files" >&2; exit 1; }
    find "$upload" -type f ! -name '*.x' -exec chmod -- 600 {} +
    find "$upload" -type f -name '*.x' -exec chmod -- 700 {} +
    # Each rename publishes one verified file atomically; rmdir proves all moved.
    find "$upload" -type f -exec sh -c 'mv -f -- "$@" "$0"' "$store" {} +
    rmdir -- "$upload"
fi
# Pin every needed file with a hard link in a private directory of the store
# (always the same filesystem), then check and copy from the pins: content
# removed from the store afterwards cannot change what this run decided.
pins="$work/.pins"
mkdir -- "$pins"
ln -s -- "$pins" "$work/.mj-store"
missing=
if [ -f "$work/.mj-keys" ]; then
    (cd "$store" && xargs sh -c '[ "$#" -eq 0 ] || ln -- "$@" "$0"' "$pins") < "$work/.mj-keys" || :
    while IFS= read -r key; do
        [ -f "$pins/$key" ] || missing="$missing$key
"
    done < "$work/.mj-keys"
fi
if [ -n "$missing" ]; then
    printf '%s' "$missing"
    exit 75
fi
# Materialization uses these private pins after releasing the store owner.
printf '%s\n' "${work##*/}" > "$work/.published"
mj_profile_quiescent
trap - EXIT"#;

const MATERIALIZE_PROFILE_CONTENT: &str = r#"set -eu
umask 077
work="$HOME/$1"; destination=$2; pins="$work/.pins"
mj_profile_claim
mj_profile_check_marker "$work/.published"
[ -d "$work/profile" ] && [ -d "$pins" ] && [ -f "$work/.mj-keys" ] || { mj_profile_stage_missing; exit "$mj_profile_stage_missing_status"; }
while IFS= read -r key; do
    [ -f "$pins/$key" ] || { mj_profile_stage_missing; exit "$mj_profile_stage_missing_status"; }
done < "$work/.mj-keys"
trap 'rm -rf -- "$work" "$pins"' EXIT
cp -RL -- "$work/profile" "$destination" || { echo "could not copy stored profile content into $destination" >&2; exit 1; }
mj_profile_quiescent"#;

struct ProfileArchiveEntry {
    path: PathBuf,
    directory: bool,
    key: Option<String>,
}

fn profile_archive_entries(
    source: &Path,
    shared_paths: &[&str],
) -> Result<Vec<ProfileArchiveEntry>> {
    fn visit(
        source: &Path,
        path: &Path,
        shared_paths: &[&str],
        entries: &mut Vec<ProfileArchiveEntry>,
    ) -> Result<()> {
        let absolute = source.join(path);
        // Like the previous tar builder, materialize staged links.
        let metadata = std::fs::metadata(&absolute)?;
        let directory = metadata.is_dir();
        let key = if !directory && shared_paths.iter().any(|shared| path.starts_with(shared)) {
            let mut header = tar::Header::new_gnu();
            header.set_metadata(&metadata);
            let digest = mj_core::worker_launch::worker_executable_digest(&absolute)?;
            Some(if header.mode()? & 0o100 != 0 {
                format!("{digest}.x")
            } else {
                digest
            })
        } else {
            None
        };
        entries.push(ProfileArchiveEntry {
            path: path.to_owned(),
            directory,
            key,
        });
        if directory {
            for child in std::fs::read_dir(&absolute)? {
                visit(
                    source,
                    &path.join(child?.file_name()),
                    shared_paths,
                    entries,
                )?;
            }
        }
        Ok(())
    }
    let mut entries = Vec::new();
    visit(source, Path::new(""), shared_paths, &mut entries)?;
    Ok(entries)
}

fn profile_content_keys(output: &[u8], expected: &HashSet<String>) -> Result<HashSet<String>> {
    let mut keys = HashSet::new();
    for key in std::str::from_utf8(output)
        .context("decode missing profile content")?
        .lines()
    {
        ensure!(
            expected.contains(key),
            "host reported unexpected profile content key {key:?}"
        );
        keys.insert(key.to_owned());
    }
    Ok(keys)
}

fn build_profile_archive(
    source: &Path,
    entries: &[ProfileArchiveEntry],
    missing: &HashSet<String>,
) -> Result<File> {
    use std::io::Seek;
    let mut file = tempfile::tempfile().context("create private profile archive")?;
    let gzip = flate2::write::GzEncoder::new(&mut file, flate2::Compression::fast());
    let mut archive = tar::Builder::new(gzip);
    archive.follow_symlinks(true);
    let mut sent = HashSet::new();
    for entry in entries {
        // Profile names live under `profile/`, so no profile file can collide
        // with `.mj-content` or `.mj-keys` at the archive root.
        let path = Path::new("profile").join(&entry.path);
        let path = path.as_path();
        let absolute = source.join(&entry.path);
        if entry.directory {
            archive.append_dir(path, absolute)?;
        } else if let Some(key) = &entry.key {
            if missing.contains(key) && sent.insert(key) {
                archive.append_path_with_name(&absolute, format!(".mj-content/{key}"))?;
            }
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            header.set_mode(0o700);
            // Relative to the link's directory: one `..` per path component
            // reaches the work directory that holds `.mj-store`.
            let depth = entry.path.components().count();
            archive.append_link(
                &mut header,
                path,
                format!("{}.mj-store/{key}", "../".repeat(depth)),
            )?;
        } else {
            archive.append_path_with_name(absolute, path)?;
        }
    }
    let mut keys: Vec<_> = entries
        .iter()
        .filter_map(|entry| entry.key.as_deref())
        .collect();
    keys.sort_unstable();
    keys.dedup();
    let list: String = keys.iter().map(|key| format!("{key}\n")).collect();
    let mut header = tar::Header::new_gnu();
    header.set_size(list.len() as u64);
    header.set_mode(0o600);
    header.set_cksum();
    archive.append_data(&mut header, ".mj-keys", list.as_bytes())?;
    archive
        .into_inner()?
        .finish()
        .context("finish profile archive")?;
    file.rewind()?;
    Ok(file)
}

/// One transfer per shared content set; unrelated profiles transfer together.
/// Only querying/collecting and publishing/pinning share the store-wide gate.
/// The destination is staging only: callers copy it into a private profile,
/// then remove it.
fn upload_profile_archive(
    executor: &impl CommandExecutor,
    ssh: &SshTarget,
    source: &Path,
    destination: &str,
    shared_paths: &[&str],
) -> Result<()> {
    let entries = profile_archive_entries(source, shared_paths)?;
    let keys: HashSet<_> = entries
        .iter()
        .filter_map(|entry| entry.key.clone())
        .collect();
    let host = targets::TargetLocator::SshBare {
        ssh: ssh.clone(),
        worker_id: None,
        workspace: String::new(),
    };
    let mut sorted_keys: Vec<_> = keys.iter().map(String::as_str).collect();
    sorted_keys.sort_unstable();
    let content_digest = mj_core::hex::lower_hex(Sha256::digest(sorted_keys.join("\n").as_bytes()));
    let gate =
        super::store::store_gate(&host, &format!("{PROFILE_CONTENT_STORE}/{content_digest}"));
    let store_gate = super::store::store_gate(&host, PROFILE_CONTENT_STORE);
    let registry_key = super::store::store_key(&host, PROFILE_CONTENT_STORE);
    let query_missing = || -> Result<HashSet<String>> {
        let mut input = std::io::Cursor::new(
            sorted_keys
                .iter()
                .map(|key| format!("{key}\n"))
                .collect::<String>(),
        );
        let _store = super::store::lock_gate(
            executor,
            &store_gate,
            destination,
            "query and collect profile content",
        )?;
        let mut args = vec![
            "sh".to_owned(),
            "-c".into(),
            profile_script(QUERY_PROFILE_CONTENT),
            "mj-profile-content".into(),
            PROFILE_CONTENT_STORE.into(),
        ];
        args.extend(ProfileUpload::active(&registry_key));
        let query = targets::ssh_command(SshTrafficClass::Control, ssh, args)
            .purpose("query host profile content")
            .warn_on_stderr()
            .stage(ProvisionStage::Syncing);
        let output = executor.execute_with_stdin(&query, &mut input)?;
        let output = targets::checked_command_output(&query, output)?;
        profile_content_keys(&output.stdout, &keys)
    };
    let mut missing = query_missing()?;
    // Cached profiles send only private bytes and need no content owner. If
    // content is missing, one uploader decides again after taking its owner:
    // the preceding uploader may already have supplied everything it needs.
    let mut content_owner = if missing.is_empty() {
        None
    } else {
        Some(super::store::lock_gate(
            executor,
            &gate,
            destination,
            "send profile content",
        )?)
    };
    if content_owner.is_some() {
        missing = query_missing()?;
        if missing.is_empty() {
            drop(content_owner.take());
        }
    }
    for attempt in 0..2 {
        let mut file = build_profile_archive(source, &entries, &missing)?;
        tracing::info!(
            archive_bytes = file.metadata()?.len(),
            shared_content_files = missing.len(),
            destination,
            "send profile content"
        );
        let token = crate::session_manager::new_command_id("profile-stage")?;
        let work = format!("{PROFILE_CONTENT_STORE}/.stage-{token}");
        let _active = {
            let _store = super::store::lock_gate(
                executor,
                &store_gate,
                destination,
                "register private profile staging",
            )?;
            ProfileUpload::register(&registry_key, &token)
        };
        let transfer = (|| -> Result<CommandOutput> {
            let command = targets::ssh_command(
                SshTrafficClass::Bulk,
                ssh,
                [
                    "sh",
                    "-c",
                    &profile_script(STAGE_PROFILE_CONTENT),
                    "mj-profile-stage",
                    &work,
                ],
            )
            .purpose("send profile content")
            .stage(ProvisionStage::Syncing);
            // The byte stream takes as long as the link needs; an operation
            // deadline would cut a progressing transfer off (#1571). A
            // cancellation still stops it.
            let output = executor
                .execute_with_stdin_without_deadline(&command, &mut file)
                .context("stream staged harness profile archive")?;
            targets::checked_command_output(&command, output)?;
            // Collection cannot unlink between publication and pinning. The
            // network transfer has already finished before taking this owner.
            let _store = super::store::lock_gate(
                executor,
                &store_gate,
                destination,
                "publish and pin profile content",
            )?;
            let output = executor.execute(
                &targets::ssh_command(
                    SshTrafficClass::Bulk,
                    ssh,
                    [
                        "sh",
                        "-c",
                        &profile_script(INSTALL_PROFILE_CONTENT),
                        "mj-profile-archive",
                        destination,
                        PROFILE_CONTENT_STORE,
                        &work,
                    ],
                )
                .purpose("publish profile content")
                .stage(ProvisionStage::Syncing),
            )?;
            drop(_store);
            if output.status != 0 {
                return Ok(output);
            }
            let output = executor.execute(
                &targets::ssh_command(
                    SshTrafficClass::Bulk,
                    ssh,
                    [
                        "sh",
                        "-c",
                        &profile_script(MATERIALIZE_PROFILE_CONTENT),
                        "mj-profile-materialize",
                        &work,
                        destination,
                    ],
                )
                .purpose("materialize profile content")
                .stage(ProvisionStage::Syncing),
            )?;
            Ok(output)
        })();
        if !transfer.as_ref().is_ok_and(|output| output.status == 0) {
            tracing::warn!(%work, "profile preparation failed; collector will reclaim staging after verified quiescence");
        }
        let output = transfer?;
        if output.status == 0 {
            return Ok(());
        }
        if attempt == 0
            && matches!(
                output.status,
                PROFILE_CONTENT_MISSING | PROFILE_STAGE_MISSING
            )
        {
            if output.status == PROFILE_STAGE_MISSING {
                // Another daemon may collect the completed prior phase. A
                // fresh query and transfer use the existing retry slot.
                missing = query_missing()?;
            } else {
                missing = profile_content_keys(&output.stdout, &keys)?;
                ensure!(
                    !missing.is_empty(),
                    "host reported missing profile content without any keys"
                );
            }
            continue;
        }
        bail!(
            "publish profile content failed with status {}: {}{}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim(),
            String::from_utf8_lossy(&output.stdout).trim()
        );
    }
    unreachable!("both upload attempts return or report failure")
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
            fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
                mj_core::targets::DeadlinePause::none()
            }

            fn cancellation_requested_without_deadline(&self) -> bool {
                self.cancellation_requested()
            }

            fn execute_without_deadline(
                &self,
                command: &mj_core::targets::CommandSpec,
            ) -> anyhow::Result<mj_core::targets::CommandOutput> {
                self.execute(command)
            }

            fn execute_with_stdin_without_deadline(
                &self,
                command: &mj_core::targets::CommandSpec,
                input: &mut (dyn std::io::Read + Send),
            ) -> anyhow::Result<mj_core::targets::CommandOutput> {
                self.execute_with_stdin(command, input)
            }

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

#[cfg(all(test, unix))]
mod profile_archive_tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeMap;
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    struct SystemTar {
        home: PathBuf,
        uploads: RefCell<Vec<Vec<u8>>>,
        delete_before_upload: Cell<usize>,
        warnings: RefCell<Vec<String>>,
        bin: RefCell<Option<PathBuf>>,
        disconnect_after_receive: Cell<bool>,
        collect_before_publish: Cell<usize>,
    }
    impl SystemTar {
        fn new(home: &Path) -> Self {
            Self {
                home: home.to_owned(),
                uploads: RefCell::new(Vec::new()),
                delete_before_upload: Cell::new(0),
                warnings: RefCell::new(Vec::new()),
                bin: RefCell::new(None),
                disconnect_after_receive: Cell::new(false),
                collect_before_publish: Cell::new(0),
            }
        }
        fn store(&self) -> PathBuf {
            self.home.join(PROFILE_CONTENT_STORE)
        }
        fn stored_files(&self) -> Vec<PathBuf> {
            fs::read_dir(self.store())
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| {
                    !path
                        .file_name()
                        .unwrap()
                        .as_encoded_bytes()
                        .starts_with(b".")
                })
                .collect()
        }
    }
    impl CommandExecutor for SystemTar {
        fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
            mj_core::targets::DeadlinePause::none()
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            self.cancellation_requested()
        }

        fn execute_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute(command)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute_with_stdin(command, input)
        }

        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.execute_with_stdin(command, &mut std::io::empty())
        }
        fn execute_with_stdin(
            &self,
            command: &CommandSpec,
            input: &mut (dyn Read + Send),
        ) -> Result<CommandOutput> {
            let mut bytes = Vec::new();
            input.read_to_end(&mut bytes)?;
            if command.purpose == "send profile content" {
                self.uploads.borrow_mut().push(bytes.clone());
                if self.delete_before_upload.get() > 0 {
                    self.delete_before_upload
                        .set(self.delete_before_upload.get() - 1);
                    for path in self.stored_files() {
                        fs::remove_file(path)?;
                    }
                }
            }
            if command.purpose == "publish profile content" && self.collect_before_publish.get() > 0
            {
                self.collect_before_publish
                    .set(self.collect_before_publish.get() - 1);
                for entry in fs::read_dir(self.store())? {
                    let path = entry?.path();
                    if path
                        .file_name()
                        .unwrap()
                        .as_encoded_bytes()
                        .starts_with(b".stage-")
                    {
                        // The real receiver already exited quiescently. Model
                        // a different daemon collecting the inter-phase gap.
                        fs::remove_dir_all(path)?;
                    }
                }
            }
            let script = command.args.last().context("missing remote command")?;
            let mut local = CommandSpec::new("sh", ["-c", script]);
            local
                .env
                .insert("HOME".into(), self.home.to_string_lossy().into_owned());
            local.cwd = Some(self.home.clone());
            if let Some(bin) = self.bin.borrow().as_ref() {
                local.env.insert(
                    "PATH".into(),
                    format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
                );
            }
            let mut output = mj_core::targets::ProcessExecutor
                .execute_with_stdin(&local, &mut std::io::Cursor::new(bytes))?;
            if command.purpose == "send profile content"
                && self.disconnect_after_receive.replace(false)
            {
                let pid = std::process::id();
                let birth = mj_core::subprocess::process_birth_identity(pid)?;
                for entry in fs::read_dir(self.store())? {
                    let path = entry?.path();
                    if path
                        .file_name()
                        .unwrap()
                        .as_encoded_bytes()
                        .starts_with(b".stage-")
                    {
                        fs::write(path.join(".owner"), format!("{pid}\n{birth}\n"))?;
                    }
                }
                output.status = 255;
                output.stderr = b"simulated SSH connection loss with a surviving receiver".to_vec();
            }
            self.warnings
                .borrow_mut()
                .push(String::from_utf8_lossy(&output.stderr).into_owned());
            Ok(output)
        }
    }

    fn upload(executor: &SystemTar, source: &Path, destination: &Path) -> Result<()> {
        upload_profile_archive(
            executor,
            &SshTarget {
                destination: "unused".into(),
                ssh_args: vec![],
            },
            source,
            destination.to_str().unwrap(),
            &shared_profile_paths(HarnessKind::Claude),
        )
    }

    // Hard-won: 73052bca7: an unrelated profile transfer held the host-wide gate until restart timed out.
    #[test]
    fn distinct_profile_content_transfers_overlap_and_identical_content_is_sent_once() {
        use std::sync::mpsc;
        struct HeldTar {
            tar: SystemTar,
            reached: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
        }
        impl CommandExecutor for HeldTar {
            fn pause_deadline(&self) -> targets::DeadlinePause {
                targets::DeadlinePause::none()
            }
            fn cancellation_requested_without_deadline(&self) -> bool {
                false
            }
            fn execute_without_deadline(&self, c: &CommandSpec) -> Result<CommandOutput> {
                self.execute(c)
            }
            fn execute_with_stdin_without_deadline(
                &self,
                c: &CommandSpec,
                i: &mut (dyn Read + Send),
            ) -> Result<CommandOutput> {
                self.execute_with_stdin(c, i)
            }
            fn execute(&self, c: &CommandSpec) -> Result<CommandOutput> {
                self.tar.execute(c)
            }
            fn execute_with_stdin(
                &self,
                c: &CommandSpec,
                i: &mut (dyn Read + Send),
            ) -> Result<CommandOutput> {
                if c.purpose == "send profile content" {
                    self.reached.send(()).unwrap();
                    self.release
                        .recv_timeout(Duration::from_secs(20))
                        .expect("test did not release transfer");
                }
                self.tar.execute_with_stdin(c, i)
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let first = home.join("first");
        let second = home.join("second");
        for (source, byte) in [(&first, b'a'), (&second, b'b')] {
            fs::create_dir_all(source.join("plugins")).unwrap();
            let mut state = u64::from(byte);
            let payload: Vec<_> = (0..100_000)
                .map(|_| {
                    state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                    (state >> 32) as u8
                })
                .collect();
            fs::write(source.join("plugins/content"), payload).unwrap();
        }
        let ssh = SshTarget {
            destination: "unused".into(),
            ssh_args: vec![],
        };
        let (reached_tx, reached_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        std::thread::scope(|scope| {
            let first_ref = &first;
            let ssh_ref = &ssh;
            let owner = scope.spawn(move || {
                let held = HeldTar {
                    tar: SystemTar::new(home),
                    reached: reached_tx,
                    release: release_rx,
                };
                upload_profile_archive(
                    &held,
                    ssh_ref,
                    first_ref,
                    home.join("held").to_str().unwrap(),
                    &["plugins"],
                )
                .unwrap();
                assert!(held.tar.uploads.borrow()[0].len() > 64 * 1024);
            });
            reached_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let (done_tx, done_rx) = mpsc::channel();
            let independent = scope.spawn(move || {
                let tar = SystemTar::new(home);
                upload_profile_archive(
                    &tar,
                    ssh_ref,
                    &second,
                    home.join("independent").to_str().unwrap(),
                    &["plugins"],
                )
                .unwrap();
                done_tx.send(()).unwrap();
            });
            let same = scope.spawn(|| {
                let tar = SystemTar::new(home);
                upload_profile_archive(
                    &tar,
                    &ssh,
                    &first,
                    home.join("same").to_str().unwrap(),
                    &["plugins"],
                )
                .unwrap();
                let archives = tar.uploads.borrow();
                let mut archive =
                    tar::Archive::new(flate2::read::GzDecoder::new(archives[0].as_slice()));
                assert!(
                    archive.entries().unwrap().all(|entry| !entry
                        .unwrap()
                        .path()
                        .unwrap()
                        .starts_with(".mj-content")),
                    "identical content was transferred twice"
                );
            });
            let overlapped = done_rx.recv_timeout(Duration::from_secs(10)).is_ok();
            release_tx.send(()).unwrap();
            independent.join().unwrap();
            owner.join().unwrap();
            same.join().unwrap();
            assert!(
                overlapped,
                "unrelated content queued behind a held transfer"
            );
        });
        // Already cached shared content must not serialize private archives,
        // even when both profiles have the exact same content-key set.
        let (reached_tx, reached_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        std::thread::scope(|scope| {
            let first_ref = &first;
            let ssh_ref = &ssh;
            let owner = scope.spawn(move || {
                let held = HeldTar {
                    tar: SystemTar::new(home),
                    reached: reached_tx,
                    release: release_rx,
                };
                upload_profile_archive(
                    &held,
                    ssh_ref,
                    first_ref,
                    home.join("cached-held").to_str().unwrap(),
                    &["plugins"],
                )
                .unwrap();
            });
            reached_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let (done_tx, done_rx) = mpsc::channel();
            let cached = scope.spawn(move || {
                let tar = SystemTar::new(home);
                upload_profile_archive(
                    &tar,
                    ssh_ref,
                    first_ref,
                    home.join("cached-parallel").to_str().unwrap(),
                    &["plugins"],
                )
                .unwrap();
                done_tx.send(()).unwrap();
            });
            let overlapped = done_rx.recv_timeout(Duration::from_secs(10)).is_ok();
            release_tx.send(()).unwrap();
            owner.join().unwrap();
            cached.join().unwrap();
            assert!(
                overlapped,
                "cached private archives queued behind each other"
            );
        });
        assert_eq!(snapshot(&home.join("same")), snapshot(&first));
    }

    // Hard-won: efbd5c927: interrupted profile receivers left private staging outside collection.
    #[test]
    fn profile_collection_reclaims_failed_and_abandoned_staging_but_preserves_owners() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("settings.json"), vec![b'x'; 100_000]).unwrap();
        let executor = SystemTar::new(temp.path());
        executor.disconnect_after_receive.set(true);
        let failure = upload(
            &executor,
            &source,
            &temp.path().join("disconnected-profile"),
        )
        .unwrap_err();
        assert!(format!("{failure:#}").contains("simulated SSH connection loss"));
        let interrupted = fs::read_dir(executor.store())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .as_encoded_bytes()
                    .starts_with(b".stage-")
            })
            .expect("SSH status 255 deleted a surviving receiver's files");
        assert!(interrupted.join("profile/settings.json").exists());
        // Finish the simulated receiver by restoring its completed real owner.
        fs::copy(interrupted.join(".quiescent"), interrupted.join(".owner")).unwrap();
        executor.collect_before_publish.set(1);
        let before = executor.uploads.borrow().len();
        let recovered = temp.path().join("automatically-recovered");
        upload(&executor, &source, &recovered).unwrap();
        assert_eq!(snapshot(&recovered), snapshot(&source));
        assert_eq!(
            executor.uploads.borrow().len() - before,
            2,
            "collector loss did not restart from transfer"
        );
        executor.collect_before_publish.set(2);
        let before = executor.uploads.borrow().len();
        let repeated = upload(&executor, &source, &temp.path().join("repeated-loss")).unwrap_err();
        assert!(format!("{repeated:#}").contains("stage removed by a collector"));
        assert_eq!(
            executor.uploads.borrow().len() - before,
            2,
            "collector loss changed the existing retry count"
        );
        let entries = profile_archive_entries(&source, &[]).unwrap();
        let compressed = build_profile_archive(&source, &entries, &HashSet::new()).unwrap();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::none());
        std::io::copy(&mut flate2::read::GzDecoder::new(compressed), &mut encoder).unwrap();
        let archive = encoder.finish().unwrap();
        assert!(archive.len() > 64 * 1024);
        let ssh = SshTarget {
            destination: "unused".into(),
            ssh_args: vec![],
        };
        let stage = |token: &str, bytes: &[u8]| {
            let work = format!("{PROFILE_CONTENT_STORE}/.stage-{token}");
            executor
                .execute_with_stdin(
                    &targets::ssh_command(
                        SshTrafficClass::Bulk,
                        &ssh,
                        [
                            "sh",
                            "-c",
                            &profile_script(STAGE_PROFILE_CONTENT),
                            "mj-profile-stage",
                            &work,
                        ],
                    )
                    .purpose("send profile content"),
                    &mut std::io::Cursor::new(bytes),
                )
                .unwrap()
        };
        assert_ne!(stage("failed", &archive[..archive.len() - 1024]).status, 0);
        assert!(
            !executor.store().join(".stage-failed").exists(),
            "receiver failure left private files"
        );
        assert_eq!(stage("collected-between-phases", &archive).status, 0);
        fs::rename(
            executor.store().join(".stage-collected-between-phases"),
            executor.store().join(".retired-between-phases"),
        )
        .unwrap();
        let destination = temp.path().join("must-not-publish");
        let removed = executor
            .execute(
                &targets::ssh_command(
                    SshTrafficClass::Bulk,
                    &ssh,
                    [
                        "sh",
                        "-c",
                        &profile_script(INSTALL_PROFILE_CONTENT),
                        "mj-profile-archive",
                        destination.to_str().unwrap(),
                        PROFILE_CONTENT_STORE,
                        &format!("{PROFILE_CONTENT_STORE}/.stage-collected-between-phases"),
                    ],
                )
                .purpose("publish profile content"),
            )
            .unwrap();
        assert_eq!(removed.status, PROFILE_STAGE_MISSING);
        assert!(String::from_utf8_lossy(&removed.stderr).contains("stage removed by a collector"));
        assert!(
            !destination.exists(),
            "a missing receive was published as an empty profile"
        );
        assert!(
            !executor
                .store()
                .join(".stage-collected-between-phases")
                .exists()
        );
        assert!(
            executor
                .store()
                .join(".retired-between-phases/profile/settings.json")
                .exists()
        );
        for token in [
            "abandoned",
            "registered",
            "live",
            "reused",
            "unknown",
            "raced",
            "unquiescent",
        ] {
            assert_eq!(stage(token, &archive).status, 0);
        }
        let pid = std::process::id();
        let birth = mj_core::subprocess::process_birth_identity(pid).unwrap();
        fs::write(
            executor.store().join(".stage-live/.owner"),
            format!("{pid}\n{birth}\n"),
        )
        .unwrap();
        fs::write(
            executor.store().join(".stage-reused/.owner"),
            format!("{pid}\n{birth}-another-process\n"),
        )
        .unwrap();
        fs::copy(
            executor.store().join(".stage-reused/.owner"),
            executor.store().join(".stage-reused/.quiescent"),
        )
        .unwrap();
        fs::remove_file(executor.store().join(".stage-unquiescent/.quiescent")).unwrap();
        let abrupt_owner = fs::read_to_string(executor.store().join(".stage-unquiescent/.owner"))
            .unwrap()
            .replace('\n', ":");
        fs::remove_file(executor.store().join(".stage-unknown/.owner")).unwrap();
        let host = targets::TargetLocator::SshBare {
            ssh,
            worker_id: None,
            workspace: String::new(),
        };
        let key = super::super::store::store_key(&host, PROFILE_CONTENT_STORE);
        let gate = super::super::store::store_gate(&host, PROFILE_CONTENT_STORE);
        let active = {
            let _store = super::super::store::lock_gate(
                &executor,
                &gate,
                "test",
                "register test profile staging",
            )
            .unwrap();
            ProfileUpload::register(&key, "registered")
        };
        let bin = temp.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let resolved_mv = targets::ProcessExecutor
            .execute(&CommandSpec::new("sh", ["-c", "command -v mv"]))
            .unwrap();
        let resolved_mv = String::from_utf8(resolved_mv.stdout).unwrap();
        // Claim first, after the collector's initial dead-owner snapshot but
        // before its rename. The changed marker must force restoration.
        let claim = profile_script("work=$1; mj_profile_claim; mj_profile_quiescent");
        mj_core::test_hooks::install_fake_command(
            &bin,
            "mv",
            &format!(
                "#!/bin/sh\n[ \"$1\" != -- ] || shift\nif [ \"${{1##*/}}\" = .stage-raced ]; then\n sh -c {} mj-test-owner \"$1\" || exit\nfi\nexec {} -- \"$@\"\n",
                targets::posix_quote(&claim),
                targets::posix_quote(resolved_mv.trim())
            ),
        );
        *executor.bin.borrow_mut() = Some(bin);
        upload(&executor, &source, &temp.path().join("profile")).unwrap();
        assert!(
            !interrupted.exists(),
            "a completed disconnected receiver was never reclaimed"
        );
        for token in ["abandoned", "reused"] {
            assert!(!executor.store().join(format!(".stage-{token}")).exists());
        }
        for token in ["registered", "live", "unknown", "raced", "unquiescent"] {
            assert!(executor.store().join(format!(".stage-{token}")).exists());
        }
        assert!(
            executor
                .warnings
                .borrow()
                .iter()
                .any(|warning| warning.contains("ownership unknown")
                    && warning.contains(".stage-unknown"))
        );
        assert!(executor.warnings.borrow().iter().any(|warning| {
            warning.contains(".stage-unquiescent") && warning.contains(&abrupt_owner)
        }));
        drop(active);
        executor.bin.borrow_mut().take();
        upload(&executor, &source, &temp.path().join("next-profile")).unwrap();
        assert!(
            !executor.store().join(".stage-raced").exists(),
            "a restored dead owner was never reclaimed"
        );
        assert!(
            !executor.store().join(".retired-between-phases").exists(),
            "an abandoned retirement was never reclaimed"
        );
        assert!(
            !executor.store().join(".stage-registered").exists(),
            "finished local owner was never reclaimed"
        );
    }

    fn snapshot(root: &Path) -> BTreeMap<PathBuf, (u32, Vec<u8>)> {
        fn visit(root: &Path, path: &Path, result: &mut BTreeMap<PathBuf, (u32, Vec<u8>)>) {
            let absolute = root.join(path);
            let metadata = fs::metadata(&absolute).unwrap();
            let bytes = if metadata.is_dir() {
                Vec::new()
            } else {
                fs::read(&absolute).unwrap()
            };
            result.insert(
                path.to_owned(),
                (metadata.permissions().mode() & 0o700, bytes),
            );
            if metadata.is_dir() {
                for entry in fs::read_dir(absolute).unwrap() {
                    visit(root, &path.join(entry.unwrap().file_name()), result);
                }
            }
        }
        let mut result = BTreeMap::new();
        visit(root, Path::new(""), &mut result);
        result
    }

    fn install_private_profile(staging: &Path, destination: &Path) {
        fs::create_dir(destination).unwrap();
        execute_checked(
            &targets::ProcessExecutor,
            CommandSpec::new(
                "cp",
                [
                    "-R",
                    staging.join(".").to_str().unwrap(),
                    destination.to_str().unwrap(),
                ],
            ),
        )
        .unwrap();
        fs::remove_dir_all(staging).unwrap();
        execute_checked(
            &targets::ProcessExecutor,
            CommandSpec::new(
                "chmod",
                ["-R", "--", "go-rwx", destination.to_str().unwrap()],
            ),
        )
        .unwrap();
    }

    // Hard-won: #1483: recursive scp spent 162 seconds on a 17 MB profile.
    // Hard-won: #1187: BSD chmod treats -- after the mode as a filename.
    #[test]
    fn streamed_profile_roundtrips_through_system_tar_without_per_file_commands() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("staged profile");
        let nested = source.join("plugins/日本語");
        fs::create_dir_all(&nested).unwrap();
        // The production archive is over 8 MiB: exercise pipe backpressure too.
        let mut payload = vec![0; 8 * 1024 * 1024 + 70_000];
        getrandom::fill(&mut payload).unwrap();
        for (name, mode) in [
            ("tool data", 0o755),
            ("line\nbreak", 0o644),
            ("-leading dash", 0o644),
        ] {
            fs::write(nested.join(name), &payload).unwrap();
            fs::set_permissions(nested.join(name), fs::Permissions::from_mode(mode)).unwrap();
        }
        fs::create_dir_all(source.join("skills/empty")).unwrap();
        fs::create_dir(source.join("empty")).unwrap();
        fs::write(source.join(".credentials.json"), b"synthetic credentials").unwrap();
        // Hard-won: review of 7954fde2e: profile files named like the
        // archive's own manifest and content directory must survive.
        fs::write(source.join(".mj-keys"), b"a profile file").unwrap();
        fs::write(source.join(".mj-content"), b"another profile file").unwrap();
        symlink("plugins/日本語/tool data", source.join("linked plugin")).unwrap();
        let executor = SystemTar::new(temp.path());
        let expected = snapshot(&source);
        for index in 0..2 {
            let staging = temp.path().join(format!("remote staging {index}"));
            let destination = temp.path().join(format!("private profile {index}"));
            let abandoned = executor.store().join("0".repeat(64));
            let abandoned_upload = executor.store().join(".upload-abandoned");
            if index == 1 {
                fs::write(&abandoned, b"unused content").unwrap();
                fs::write(&abandoned_upload, b"interrupted publication").unwrap();
                let mut args = vec!["-t".to_owned(), "200001010000".to_owned()];
                args.extend(
                    fs::read_dir(executor.store())
                        .unwrap()
                        .map(|entry| entry.unwrap().path().to_string_lossy().into_owned()),
                );
                execute_checked(&targets::ProcessExecutor, CommandSpec::new("touch", args))
                    .unwrap();
            }
            upload(&executor, &source, &staging).unwrap();
            if index == 1 {
                assert!(!abandoned.exists());
                assert!(!abandoned_upload.exists());
            }
            for (path, (mode, _)) in &expected {
                assert_eq!(
                    staging.join(path).metadata().unwrap().permissions().mode() & 0o777,
                    *mode,
                    "staging permissions: {}",
                    path.display()
                );
            }
            install_private_profile(&staging, &destination);
            assert_eq!(snapshot(&destination), expected);
            for (path, (mode, _)) in &expected {
                let metadata = fs::symlink_metadata(destination.join(path)).unwrap();
                assert!(!metadata.file_type().is_symlink());
                assert_eq!(metadata.permissions().mode() & 0o777, *mode);
                for stored in executor.stored_files() {
                    assert_ne!(metadata.ino(), stored.metadata().unwrap().ino());
                }
            }
            // A write to a private session copy cannot corrupt later installs.
            fs::write(
                destination.join("plugins/日本語/tool data"),
                b"session mutation",
            )
            .unwrap();
        }
        let uploads = executor.uploads.borrow();
        assert_eq!(uploads.len(), 2);
        for (index, bytes) in uploads.iter().enumerate() {
            let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes.as_slice()));
            let shared_content: Vec<_> = archive
                .entries()
                .unwrap()
                .map(|entry| entry.unwrap().path().unwrap().into_owned())
                .filter(|path| path.starts_with(".mj-content"))
                .collect();
            assert_eq!(shared_content.len(), if index == 0 { 2 } else { 0 });
        }
        let store = executor.stored_files();
        assert_eq!(store.len(), 2);
        for path in store {
            assert_eq!(fs::read(&path).unwrap(), payload);
            let executable = path.extension().is_some_and(|extension| extension == "x");
            assert_eq!(
                path.metadata().unwrap().permissions().mode() & 0o777,
                if executable { 0o700 } else { 0o600 }
            );
        }
    }

    #[test]
    fn profile_content_disappearing_after_query_retries_once_with_the_missing_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(source.join("plugins")).unwrap();
        fs::write(source.join("plugins/content"), vec![b'x'; 100_000]).unwrap();
        fs::write(source.join("settings.json"), b"private settings").unwrap();
        let executor = SystemTar::new(temp.path());
        upload(&executor, &source, &temp.path().join("initial")).unwrap();
        executor.delete_before_upload.set(1);
        let recovered = temp.path().join("recovered");
        upload(&executor, &source, &recovered).unwrap();
        assert_eq!(snapshot(&recovered), snapshot(&source));
        assert_eq!(executor.uploads.borrow().len(), 3);
        // The real link script reports exactly which content disappeared.
        let key = executor
            .stored_files()
            .pop()
            .unwrap()
            .file_name()
            .unwrap()
            .to_owned();
        let entries = profile_archive_entries(&source, &["plugins"]).unwrap();
        let mut archive = build_profile_archive(&source, &entries, &HashSet::new()).unwrap();
        fs::remove_file(executor.store().join(&key)).unwrap();
        let destination = temp.path().join("missing");
        let work = Path::new("missing-work");
        executor
            .execute_with_stdin(
                &targets::ssh_command(
                    SshTrafficClass::Bulk,
                    &SshTarget {
                        destination: "unused".into(),
                        ssh_args: vec![],
                    },
                    [
                        "sh",
                        "-c",
                        &profile_script(STAGE_PROFILE_CONTENT),
                        "mj-profile-stage",
                        work.to_str().unwrap(),
                    ],
                )
                .purpose("send profile content"),
                &mut archive,
            )
            .unwrap();
        let command = targets::ssh_command(
            SshTrafficClass::Control,
            &SshTarget {
                destination: "unused".into(),
                ssh_args: vec![],
            },
            [
                "sh",
                "-c",
                &profile_script(INSTALL_PROFILE_CONTENT),
                "mj-profile-archive",
                destination.to_str().unwrap(),
                PROFILE_CONTENT_STORE,
                work.to_str().unwrap(),
            ],
        )
        .purpose("publish profile content");
        let output = executor.execute(&command).unwrap();
        assert_eq!(output.status, PROFILE_CONTENT_MISSING);
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            key.to_str().unwrap()
        );
    }

    // Hard-won: review of 09769fc85: a stored file that exists but cannot be
    // copied must fail the upload instead of leaving a profile with gaps.
    #[test]
    fn stored_profile_content_that_cannot_be_copied_fails_the_upload() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(source.join("plugins")).unwrap();
        fs::write(source.join("plugins/content"), vec![b'x'; 100_000]).unwrap();
        let executor = SystemTar::new(temp.path());
        upload(&executor, &source, &temp.path().join("initial")).unwrap();
        let stored = executor.stored_files().pop().unwrap();
        fs::set_permissions(&stored, fs::Permissions::from_mode(0o000)).unwrap();
        let error = upload(&executor, &source, &temp.path().join("unreadable"))
            .unwrap_err()
            .to_string();
        fs::set_permissions(&stored, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(
            error.contains("could not copy stored profile content"),
            "{error}"
        );
    }
}
