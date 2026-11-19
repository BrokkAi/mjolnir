//! From a session's recorded target to the locator its commands run against.

use super::*;

/// The execution-plan locator for a session's stored target.
///
/// The mapping itself lives in `mj_core::targets`; this only pairs the stored
/// locator with the target template the session was created against.
pub fn backend_locator(
    locator: &mj_core::state::TargetLocator,
    session: &mj_core::state::SessionRecord,
    config: &mj_core::config::Config,
) -> Result<TargetLocator> {
    recorded_backend_locator(
        locator,
        &session.id,
        &session.target_template_id,
        session.target_runtime.as_deref(),
        config,
    )
}

pub fn recorded_backend_locator(
    locator: &mj_core::state::TargetLocator,
    session_id: &str,
    target_template_id: &str,
    runtime: Option<&mj_core::state::TargetRuntimeSettings>,
    config: &mj_core::config::Config,
) -> Result<TargetLocator> {
    let runtime = if runtime.is_some() || locator_needs_connection(locator) {
        Some(mj_core::state::target_runtime_settings_for(
            session_id,
            target_template_id,
            Some(locator),
            runtime,
            config,
        )?)
    } else {
        None
    };
    Ok(TargetLocator::try_from(RecordedTarget {
        locator,
        runtime: runtime.as_deref(),
        session_id,
    })?)
}

/// The directory the session's repositories are checked out under.
/// `container_workspace` is the session record's recorded container workspace,
/// which only container locators use.
pub fn workspace_root(
    backend: &TargetLocator,
    container_workspace: Option<&std::path::Path>,
) -> String {
    match backend {
        TargetLocator::LocalPodman { .. }
        | TargetLocator::LocalDocker { .. }
        | TargetLocator::AppleContainer { .. }
        | TargetLocator::SshPodman { .. }
        | TargetLocator::SshDocker { .. }
        | TargetLocator::SshAppleContainer { .. }
        | TargetLocator::SshNitro { .. } => container_workspace_root(container_workspace),
        TargetLocator::AwsEc2 { workspace, .. } | TargetLocator::SshBare { workspace, .. } => {
            workspace.clone()
        }
        TargetLocator::LocalBare { worker_root } => worker_root.clone(),
    }
}
