use super::*;
use mj_core::hex::lower_hex;

/// Linux appends " (deleted)" to `/proc/<pid>/exe` for a removed image. That
/// marker belongs in a message but never in a decision, which `is_file` makes.
pub(super) fn display_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    text.strip_suffix(" (deleted)").unwrap_or(&text).to_owned()
}

/// The architecture a configured template names outright, if it names one. A
/// container `platform` such as `linux/arm64` decides what the target runs
/// whatever the controller's own machine is, and it is the only architecture a
/// configured target template can state: the configured `AwsEc2` variant names
/// a launch template, whose instance type is only discoverable through the AWS
/// API.
pub(super) fn template_architecture(
    template: &mj_core::config::TargetTemplate,
) -> Option<&'static str> {
    use mj_core::config::TargetTemplate as Template;
    let platform = match template {
        Template::LocalPodman { container }
        | Template::LocalDocker { container }
        | Template::AppleContainer { container }
        | Template::SshPodman { container, .. }
        | Template::SshDocker { container, .. } => container.platform.as_deref()?,
        Template::LocalBare | Template::SshBare { .. } | Template::AwsEc2 { .. } => return None,
    };
    // Platform strings appear as "linux/arm64", "arm64", or "linux/arm64/v8".
    platform
        .split('/')
        .find_map(|part| targets::normalize_architecture(part.trim()).ok())
}

/// Architectures a resume must be able to serve, knowing only the configured
/// template. Provisioning learns the real answer by running `uname -m` on the
/// live target; a resume has no target yet, so this uses what is knowable
/// without one: an architecture the template names, else the controller's own
/// architecture for a target that runs on this machine, else either Linux
/// architecture for a remote target.
pub(super) fn preflight_architectures(
    template: &mj_core::config::TargetTemplate,
) -> Vec<&'static str> {
    use mj_core::config::TargetTemplate as Template;
    if let Some(arch) = template_architecture(template) {
        return vec![arch];
    }
    match template {
        Template::LocalBare
        | Template::LocalPodman { .. }
        | Template::LocalDocker { .. }
        | Template::AppleContainer { .. } => vec![std::env::consts::ARCH],
        Template::SshBare { .. }
        | Template::SshPodman { .. }
        | Template::SshDocker { .. }
        | Template::AwsEc2 { .. } => {
            vec!["x86_64", "aarch64"]
        }
    }
}

/// Resolve the worker before expensive provisioning or transcript compaction.
/// Existing SSH bare hosts are probed; disposable targets use their template.
///
/// A resume compacts a cross-harness transcript before it provisions anything,
/// which costs minutes and paid model requests. Resolving the worker binary is
/// performed before target creation, so a resume that could never install a worker
/// must fail before spending any of that. Remote sources are downloaded and
/// verified here too, before provisioning can create a container.
pub(in crate::controller) fn preflight_worker_binary(
    template: &mj_core::config::TargetTemplate,
    executor: &impl CommandExecutor,
) -> Result<()> {
    if let mj_core::config::TargetTemplate::SshBare { ssh, .. } = template {
        let command = targets::ssh_command(&SshTarget::from(ssh), ["uname", "-sm"])
            .purpose("detect target platform");
        let platform = probe_platform(executor, command)?;
        return materialize_worker_source(worker_binary_for_arch(
            platform.architecture,
            WorkerBinaryRequirement::for_os(platform.os),
        )?)
        .map(|_| ());
    }
    // Existing SSH bare hosts were resolved above. Containers and the
    // disposable EC2 backend require portable Linux workers.
    let requirement = if matches!(template, mj_core::config::TargetTemplate::LocalBare) {
        WorkerBinaryRequirement::LocalHost
    } else {
        WorkerBinaryRequirement::PortableLinux
    };
    let mut failure = None;
    for arch in preflight_architectures(template) {
        match worker_binary_for_arch(arch, requirement).and_then(materialize_worker_source) {
            Ok(_) => return Ok(()),
            Err(error) => failure = Some(error),
        }
    }
    match failure {
        // The message is the one provisioning would have printed later, so the
        // user reads the same fix, sooner.
        Some(error) => Err(error).context("preflight the worker binary before provisioning"),
        None => Ok(()),
    }
}

/// Why no worker binary could serve a session on this target, or `None` when
/// one can (or when that cannot be told without a host that does not answer).
///
/// This is the resolution `preflight_worker_binary` performs, without the
/// download or the build check, so the daemon can refuse a launch up front
/// instead of admitting it and failing the session seconds later. An
/// existing SSH host is asked its platform; a host that does not answer, or
/// answers with a platform Mjolnir does not support, is not a worker-source
/// problem and is left to the launch to report.
pub(crate) fn worker_source_problem(
    template: &mj_core::config::TargetTemplate,
    executor: &impl CommandExecutor,
) -> Option<String> {
    if let mj_core::config::TargetTemplate::SshBare { ssh, .. } = template {
        let command = targets::ssh_command(&SshTarget::from(ssh), ["uname", "-sm"])
            .purpose("detect target platform");
        let platform = probe_platform(executor, command).ok()?;
        return worker_binary_for_arch(
            platform.architecture,
            WorkerBinaryRequirement::for_os(platform.os),
        )
        .err()
        .map(|error| format!("{error:#}"));
    }
    let requirement = if matches!(template, mj_core::config::TargetTemplate::LocalBare) {
        WorkerBinaryRequirement::LocalHost
    } else {
        WorkerBinaryRequirement::PortableLinux
    };
    let mut failure = None;
    for arch in preflight_architectures(template) {
        match worker_binary_for_arch(arch, requirement) {
            Ok(_) => return None,
            Err(error) => failure = Some(format!("{error:#}")),
        }
    }
    failure
}

/// The worker an existing SSH host needs, with the target triple it is
/// named by, read from the host's own platform. `None` for a template that
/// is not a bare SSH host and for a host that does not say what it runs; the
/// SSH reachability check reports the latter.
pub fn ssh_worker_binary_prerequisite(
    template: &mj_core::config::TargetTemplate,
    executor: &impl CommandExecutor,
) -> Option<(String, Result<WorkerBinaryAvailability>)> {
    let mj_core::config::TargetTemplate::SshBare { ssh, .. } = template else {
        return None;
    };
    let command = targets::ssh_command(&SshTarget::from(ssh), ["uname", "-sm"])
        .purpose("detect target platform");
    let platform = probe_platform(executor, command).ok()?;
    let requirement = WorkerBinaryRequirement::for_os(platform.os);
    Some((
        requirement.triple(platform.architecture),
        worker_binary_for_arch(platform.architecture, requirement),
    ))
}

pub(in crate::controller) fn worker_binary_for(
    locator: &targets::TargetLocator,
    executor: &impl CommandExecutor,
) -> Result<PathBuf> {
    let platform = probe_platform(executor, targets::platform_probe(locator))?;
    let requirement = if matches!(locator, targets::TargetLocator::LocalBare { .. }) {
        WorkerBinaryRequirement::LocalHost
    } else {
        WorkerBinaryRequirement::for_os(platform.os)
    };
    materialize_worker_source(worker_binary_for_arch(platform.architecture, requirement)?)
}

fn materialize_worker_source(source: WorkerBinaryAvailability) -> Result<PathBuf> {
    let path = match source {
        WorkerBinaryAvailability::Local { path, .. } => Ok(path),
        WorkerBinaryAvailability::Remote {
            url,
            sha256,
            triple,
        } => download_worker(&url, &sha256, &triple),
    }?;
    verify_worker_build(&path)?;
    Ok(path)
}

fn probe_platform(
    executor: &impl CommandExecutor,
    command: CommandSpec,
) -> Result<targets::TargetPlatform> {
    let output = execute_checked(executor, command)?;
    targets::TargetPlatform::parse(std::str::from_utf8(&output.stdout)?)
}

pub(super) fn download_worker(url: &str, expected_sha256: &str, triple: &str) -> Result<PathBuf> {
    validate_worker_sha256(expected_sha256)?;
    let digest = expected_sha256.to_ascii_lowercase();
    let directory = data_dir().join("workers").join("pinned");
    let destination = directory.join(&digest).join("hel");
    std::fs::create_dir_all(destination.parent().unwrap_or(&directory))?;
    if destination.is_file() {
        let bytes = std::fs::read(&destination).with_context(|| {
            format!(
                "read cached worker for {triple} from {}",
                destination.display()
            )
        })?;
        if lower_hex(Sha256::digest(&bytes)).eq_ignore_ascii_case(expected_sha256) {
            verify_worker_build(&destination)?;
            return Ok(destination);
        }
        bail!(
            "content-addressed worker cache {} does not match {} checksum",
            destination.display(),
            expected_sha256
        );
    }
    let bytes = on_dedicated_thread(|| {
        Ok(reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()?
            .get(url)
            .send()?
            .error_for_status()?
            .bytes()?)
    })?;
    let actual = lower_hex(Sha256::digest(&bytes));
    if !actual.eq_ignore_ascii_case(expected_sha256) {
        bail!("downloaded worker checksum mismatch: expected {expected_sha256}, got {actual}");
    }
    std::fs::create_dir_all(&directory)?;
    let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
    std::io::Write::write_all(&mut temporary, &bytes)?;
    temporary.as_file_mut().sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o700))?;
    }
    publish_cached_worker(temporary, &directory, &digest)
}

pub(super) fn validate_worker_sha256(expected_sha256: &str) -> Result<()> {
    if expected_sha256.len() != 64 || !expected_sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("MJ_WORKER_SHA256 must be a 64-character hexadecimal digest");
    }
    Ok(())
}
