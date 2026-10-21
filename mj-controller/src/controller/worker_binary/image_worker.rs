//! The worker an agent-dev image carries, and the choice between it and a
//! host binary.

use super::*;

/// Where an agent-dev image keeps the worker built from its own commit.
pub(in crate::controller) const IMAGE_WORKER: &str = "/opt/mjolnir/mj-worker";

/// Print the build of the image's worker, or `absent` when the image has
/// none. A worker that is present but cannot run fails the command, so an
/// unusable worker is reported rather than mistaken for a missing one.
const IMAGE_WORKER_BUILD_SCRIPT: &str = "if [ -e /opt/mjolnir/mj-worker ]; then exec /opt/mjolnir/mj-worker worker build-id; fi; echo absent";

/// How a session's target gets the worker this controller needs. Either way
/// the worker carries [`WORKER_BUILD_ID`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::controller) enum WorkerInstall {
    /// The container's image carries this controller's worker build. The
    /// session's `hel` links to it and nothing is copied.
    Image,
    /// A host binary carrying this controller's build is copied in.
    Host(PathBuf),
}

impl WorkerInstall {
    /// Choose the worker for a target. A container uses its image's worker
    /// when that is this controller's build; anything else (a custom image, an
    /// older image, or a development build that changed the worker) gets the
    /// host binary, which must carry this build too.
    pub(in crate::controller) fn for_target(
        locator: &targets::TargetLocator,
        executor: &impl CommandExecutor,
    ) -> Result<Self> {
        if locator.container_engine().is_none() {
            return worker_binary_for(locator, executor).map(Self::Host);
        }
        let image_build = image_worker_build(locator, executor)?;
        if image_build.as_deref() == Some(WORKER_BUILD_ID) {
            return Ok(Self::Image);
        }
        let carried = image_build.map_or_else(
            || "no worker".to_owned(),
            |build| format!("worker build {build}"),
        );
        tracing::info!(
            image_worker = carried,
            expected = WORKER_BUILD_ID,
            "container image does not carry this controller's worker; installing the host worker"
        );
        worker_binary_for(locator, executor)
            .map(Self::Host)
            .with_context(|| {
                format!(
                    "the container's image carries {carried}, not this Mjolnir's worker build \
                     {WORKER_BUILD_ID}, and no host worker can replace it"
                )
            })
    }

    /// The host binary this install copies, if it copies one.
    pub(in crate::controller) fn host_binary(&self) -> Option<&Path> {
        match self {
            Self::Image => None,
            Self::Host(path) => Some(path),
        }
    }
}

/// The worker build a container's image carries at [`IMAGE_WORKER`], or
/// `None` when the image has no worker there.
pub(in crate::controller) fn image_worker_build(
    locator: &targets::TargetLocator,
    executor: &impl CommandExecutor,
) -> Result<Option<String>> {
    let command = targets::locator_command(
        locator,
        vec!["sh".into(), "-c".into(), IMAGE_WORKER_BUILD_SCRIPT.into()],
    )
    .purpose("read the container image's worker build");
    let output = execute_checked(executor, command)?;
    parse_image_worker_build(&output.stdout)
}

fn parse_image_worker_build(stdout: &[u8]) -> Result<Option<String>> {
    let text = std::str::from_utf8(stdout)
        .context("the image's worker build is not UTF-8")?
        .trim();
    if text == "absent" {
        return Ok(None);
    }
    ensure!(
        mj_core::worker_build::is_worker_build(text),
        "the image's worker reported an unrecognized build {text:?}"
    );
    Ok(Some(text.to_owned()))
}
