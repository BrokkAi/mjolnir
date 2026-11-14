//! The Nitro Sandbox host engine is this build's worker binary, run as root
//! through `sudo -n` (`mj-worker microvm`). It reaches an EC2 host the way a
//! worker reaches an `ssh_bare` host: through the shared worker store, linked
//! into place with the same digest-checked, atomically promoted installer.

use super::*;

/// Names the host-scoped installation in store gates and logs. It is not a
/// session: the engine outlives every session on the host.
const ENGINE_OWNER: &str = "microvm-engine";

/// Make sure the host runs this build's engine, installing it when the digest
/// differs.
///
/// Every engine command goes through [`targets::engine_argv`], which names the
/// path installed here, so this runs before the first one: at launch, before
/// the image refresher pulls to the host, and by doctor. Concurrent callers
/// converge on one installation because the store publishes atomically and
/// promotion is a rename.
pub(crate) fn ensure_microvm_engine(
    ssh: &SshTarget,
    executor: &impl CommandExecutor,
) -> Result<()> {
    // Only the store and the platform probe use this locator, and they take
    // its host and the shape of its workspace; nothing is created there.
    let host = targets::TargetLocator::SshBare {
        ssh: ssh.clone(),
        worker_id: None,
        workspace: format!(".local/share/hel/workspaces/{ENGINE_OWNER}"),
    };
    let source = worker_binary_for(&host, executor)
        .context("find the worker binary for the Nitro sandbox host")?;
    let digest = mj_core::worker_launch::worker_executable_digest(&source)?;
    let destination = targets::MICROVM_ENGINE_WORKER;
    if crate::session_manager::installed_digest_matches(
        executor,
        &upgrade::installed_file_digest_command(&host, destination, "verify the host engine"),
        &digest,
    ) {
        return Ok(());
    }
    let directory = Path::new(destination)
        .parent()
        .and_then(Path::to_str)
        .context("the engine path has no directory")?;
    execute_checked(
        executor,
        targets::locator_command(&host, vec!["mkdir".into(), "-p".into(), directory.into()])
            .purpose("create the Nitro sandbox engine directory"),
    )?;
    install_worker_binary_at(
        executor,
        &host,
        ENGINE_OWNER,
        &WorkerInstall::Host(source),
        destination,
        None,
    )
    .context("install the Nitro sandbox engine on the host")
}
