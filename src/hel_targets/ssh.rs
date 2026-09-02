use super::*;

/// The connectivity probe `mj doctor` runs against an SSH target.
///
/// It reuses the provisioning argument order so the probe fails exactly where
/// a real session would, with two deliberate overrides prepended. OpenSSH
/// honours the first occurrence of an option, so these win over the
/// provisioning defaults: `BatchMode=yes` never prompts for a password, and
/// `StrictHostKeyChecking=yes` never accepts an unknown host key. Doctor
/// diagnoses; the user decides whether to trust a key.
pub fn ssh_connectivity_probe(ssh: &SshTarget) -> CommandSpec {
    let mut probe = ssh.clone();
    probe.ssh_args.splice(
        0..0,
        [
            "-o".to_owned(),
            "BatchMode=yes".to_owned(),
            "-o".to_owned(),
            "StrictHostKeyChecking=yes".to_owned(),
        ],
    );
    ssh_command(&probe, ["true"]).purpose("verify SSH connectivity")
}

pub(super) fn ssh_command(
    ssh: &SshTarget,
    args: impl IntoIterator<Item = impl AsRef<str>>,
) -> CommandSpec {
    ssh_command_owned(
        ssh,
        args.into_iter()
            .map(|arg| arg.as_ref().to_owned())
            .collect(),
    )
}

pub(super) fn ssh_command_owned(ssh: &SshTarget, remote_args: Vec<String>) -> CommandSpec {
    let mut args = ssh.ssh_args.clone();
    args.push(ssh.destination.clone());
    args.push(join_remote_command(&remote_args));
    CommandSpec::new("ssh", args)
}

pub fn join_remote_command(args: &[String]) -> String {
    args.iter()
        .map(|arg| posix_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Complete remote directory paths through the configured SSH target.
///
/// The SSH connection timeout and noninteractive mode keep a Tab press from
/// blocking the wizard when a host is unavailable. The quoted prefix remains
/// literal while the trailing glob is expanded only by the remote shell.
pub fn ssh_directory_completions(
    ssh: &SshTarget,
    prefix: &str,
    executor: &impl CommandExecutor,
) -> Result<Vec<String>> {
    if prefix.is_empty() {
        return Ok(Vec::new());
    }
    let remote_command = format!("ls -d -- {}*/ 2>/dev/null", posix_quote(prefix));
    let mut args = ssh.ssh_args.clone();
    args.extend([
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ConnectTimeout=3".into(),
        "-o".into(),
        "ServerAliveInterval=2".into(),
        "-o".into(),
        "ServerAliveCountMax=1".into(),
        ssh.destination.clone(),
        remote_command,
    ]);
    let output = executor
        .execute(&CommandSpec::new("ssh", args).purpose("complete remote mount directory"))?;
    if output.status != 0 {
        return Ok(Vec::new());
    }
    let mut matches = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|path| path.starts_with(prefix) && path.ends_with('/'))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    matches.sort();
    matches.dedup();
    Ok(matches)
}

/// Check whether a directory exists on the configured SSH host.
pub fn ssh_directory_exists(
    ssh: &SshTarget,
    path: &Path,
    executor: &impl CommandExecutor,
) -> Result<bool> {
    let command = ssh_validation_command(
        ssh,
        vec![
            "test".into(),
            "-d".into(),
            path.to_string_lossy().into_owned(),
        ],
        "validate remote directory",
    );
    let output = executor.execute(&command)?;
    match output.status {
        0 => Ok(true),
        1 => Ok(false),
        status => bail!(
            "remote directory check failed with status {status}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

/// Verify that a bare-SSH project path exists and has a committed Git HEAD.
pub fn validate_bare_project_directory(
    ssh: &SshTarget,
    path: &Path,
    executor: &impl CommandExecutor,
) -> Result<()> {
    validate_bare_project_path(path)?;
    if !ssh_directory_exists(ssh, path, executor)? {
        bail!(
            "remote project directory {} does not exist or is not a directory",
            path.display()
        );
    }
    let output = executor.execute(&ssh_validation_command(
        ssh,
        vec![
            "git".into(),
            "-C".into(),
            path.to_string_lossy().into_owned(),
            "rev-parse".into(),
            "--verify".into(),
            "HEAD".into(),
        ],
        "validate bare SSH Git project",
    ))?;
    if output.status != 0 {
        let detail = String::from_utf8_lossy(&output.stderr);
        let detail = detail.trim();
        if detail.is_empty() {
            bail!(
                "remote project directory {} has no valid Git HEAD",
                path.display()
            );
        }
        bail!(
            "remote project directory {} has no valid Git HEAD: {detail}",
            path.display()
        );
    }
    Ok(())
}

pub(super) fn validate_bare_project_path(path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| part == std::path::Component::ParentDir)
    {
        bail!("bare project directory must be an absolute safe path");
    }
    Ok(())
}

pub(super) fn ssh_validation_command(
    ssh: &SshTarget,
    remote_args: Vec<String>,
    purpose: &'static str,
) -> CommandSpec {
    let mut args = ssh.ssh_args.clone();
    args.extend([
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ConnectTimeout=3".into(),
        "-o".into(),
        "ServerAliveInterval=2".into(),
        "-o".into(),
        "ServerAliveCountMax=1".into(),
        ssh.destination.clone(),
        join_remote_command(&remote_args),
    ]);
    CommandSpec::new("ssh", args).purpose(purpose)
}

/// Wrap a value so a POSIX shell reads it as one literal argument. Used at the
/// SSH boundary here and when Hel rebuilds an agent's terminal command line
/// (`hel_terminal::shell_line`).
pub fn posix_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub(super) fn verify_locator(locator: &TargetLocator, session_id: &str) -> Result<()> {
    let expected_name = resource_name(session_id)?;
    match locator {
        TargetLocator::LocalBare { worker_root } => {
            let path = Path::new(worker_root);
            if !path.is_absolute()
                || path
                    .components()
                    .any(|part| part == std::path::Component::ParentDir)
                || !path.ends_with(session_id)
            {
                bail!("refusing cleanup: invalid local bare worker root");
            }
        }
        TargetLocator::LocalPodman { container_id }
        | TargetLocator::LocalDocker { container_id }
        | TargetLocator::AppleContainer { container_id }
        | TargetLocator::SshPodman { container_id, .. } => {
            if container_id != &expected_name && !is_runtime_container_id(container_id) {
                bail!(
                    "refusing cleanup: container locator is neither the generated name nor an immutable runtime ID"
                );
            }
        }
        TargetLocator::AwsEc2 {
            instance_id,
            workspace,
            ..
        } => {
            if !valid_ec2_instance_id(instance_id) {
                bail!("refusing cleanup: invalid EC2 instance ID");
            }
            verify_session_workspace(workspace, session_id)?;
        }
        TargetLocator::SshBare { workspace, .. } => {
            verify_session_workspace(workspace, session_id)?
        }
    }
    Ok(())
}

pub(super) fn verify_session_workspace(workspace: &str, session_id: &str) -> Result<()> {
    validate_workspace_prefix(workspace)?;
    let final_component = workspace.trim_end_matches('/').rsplit('/').next();
    if final_component != Some(session_id) {
        bail!("refusing cleanup: workspace does not end in the exact session ID");
    }
    Ok(())
}

pub(super) fn validate_session_id(value: &str) -> Result<()> {
    if value.len() < 8
        || value.len() > 128
        || !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        bail!("session ID must be 8-128 ASCII letters, digits, '-' or '_'");
    }
    Ok(())
}

pub(super) fn validate_relative_path(value: &str) -> Result<()> {
    let path = std::path::Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        bail!("unsafe relative bundle path {value:?}");
    }
    Ok(())
}

pub(super) fn validate_workspace_prefix(value: &str) -> Result<()> {
    if value.is_empty()
        || value == "/"
        || value == "~"
        || value == "~/"
        || value.contains('\0')
        || value.split('/').any(|part| part == "..")
    {
        bail!("unsafe workspace path");
    }
    Ok(())
}

pub(super) fn validate_container_template(template: &ContainerTemplate) -> Result<()> {
    if template.image.trim().is_empty() || template.image.starts_with('-') {
        bail!("invalid container image");
    }
    if template
        .extra_run_args
        .iter()
        .any(|arg| arg == "--name" || arg.starts_with("--name="))
    {
        bail!("container template may not override the generated name");
    }
    if template.extra_run_args.iter().any(|arg| {
        arg == "--label"
            || [SESSION_LABEL, MANAGED_LABEL]
                .iter()
                .any(|label| arg.starts_with(&format!("--label={label}=")))
    }) {
        bail!("container template may not override Mjolnir ownership labels");
    }
    Ok(())
}

pub(super) fn validate_ssh(ssh: &SshTarget) -> Result<()> {
    if ssh.destination.trim().is_empty()
        || ssh.destination.starts_with('-')
        || ssh.destination.chars().any(char::is_whitespace)
    {
        bail!("invalid SSH destination");
    }
    Ok(())
}

pub(super) fn validate_aws(aws: &AwsTemplate) -> Result<()> {
    validate_ssh(&aws.ssh)?;
    for (name, value) in [
        ("AWS profile", &aws.profile),
        ("AWS region", &aws.region),
        ("launch template", &aws.launch_template),
    ] {
        if value.is_empty()
            || value.starts_with('-')
            || !value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
        {
            bail!("invalid {name}");
        }
    }
    Ok(())
}

pub(super) fn validate_executable(value: &str) -> Result<()> {
    if value.is_empty() || value.starts_with('-') || value.chars().any(char::is_whitespace) {
        bail!("invalid executable name");
    }
    Ok(())
}

pub(super) fn valid_ec2_instance_id(value: &str) -> bool {
    value
        .strip_prefix("i-")
        .is_some_and(|rest| rest.len() >= 8 && rest.chars().all(|c| c.is_ascii_hexdigit()))
}

pub(super) fn is_runtime_container_id(value: &str) -> bool {
    value.len() >= 12 && value.len() <= 128 && value.chars().all(|c| c.is_ascii_hexdigit())
}
