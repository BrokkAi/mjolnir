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
    platform.split('/').find_map(|part| match part.trim() {
        "x86_64" | "amd64" => Some("x86_64"),
        "aarch64" | "arm64" => Some("aarch64"),
        _ => None,
    })
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

/// Whether this controller could produce a Linux worker binary for a target
/// that does not exist yet.
///
/// A resume compacts a cross-harness transcript before it provisions anything,
/// which costs minutes and paid model requests. Resolving the worker binary is
/// local and takes microseconds, so a resume that could never install a worker
/// must fail before spending any of that. This downloads nothing: a remote
/// source counts as available, because fetching it belongs to provisioning.
pub(in crate::controller) fn preflight_worker_binary(
    template: &mj_core::config::TargetTemplate,
) -> Result<()> {
    // Only a bare local target may run the controller's own host binary as
    // its worker; every other target needs a portable Linux worker.
    let requirement = if matches!(template, mj_core::config::TargetTemplate::LocalBare) {
        WorkerBinaryRequirement::LocalHost
    } else {
        WorkerBinaryRequirement::PortableLinux
    };
    let mut failure = None;
    for arch in preflight_architectures(template) {
        match worker_binary_for_arch(arch, requirement) {
            Ok(_) => return Ok(()),
            Err(error) => failure = Some(error),
        }
    }
    match failure {
        // The message is the one provisioning would have printed later, so the
        // user reads the same fix, sooner.
        Some(error) => Err(error).context("preflight the worker binary before resuming"),
        None => Ok(()),
    }
}

pub(in crate::controller) fn worker_binary_for(
    locator: &targets::TargetLocator,
    executor: &impl CommandExecutor,
) -> Result<PathBuf> {
    let arch = target_architecture(locator, executor)?;
    let requirement = if matches!(locator, targets::TargetLocator::LocalBare { .. }) {
        WorkerBinaryRequirement::LocalHost
    } else {
        WorkerBinaryRequirement::PortableLinux
    };
    match worker_binary_for_arch(arch, requirement)? {
        WorkerBinaryAvailability::Local { path, .. } => Ok(path),
        WorkerBinaryAvailability::Remote {
            url,
            sha256,
            triple,
        } => download_worker(&url, &sha256, &triple),
    }
}

pub(super) fn target_architecture(
    locator: &targets::TargetLocator,
    executor: &impl CommandExecutor,
) -> Result<&'static str> {
    let command = targets::locator_command(locator, vec!["uname".into(), "-m".into()])
        .purpose("detect target architecture");
    let output = execute_checked(executor, command)?;
    match String::from_utf8(output.stdout)?.trim() {
        "x86_64" | "amd64" => Ok("x86_64"),
        "aarch64" | "arm64" => Ok("aarch64"),
        architecture => bail!("unsupported target architecture {architecture:?}"),
    }
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
            return Ok(destination);
        }
        bail!(
            "content-addressed worker cache {} does not match {} checksum",
            destination.display(),
            expected_sha256
        );
    }
    let bytes = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?
        .get(url)
        .send()?
        .error_for_status()?
        .bytes()?;
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
