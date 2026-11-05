//! Where this process runs, and therefore where its harness programs come
//! from.
//!
//! The container launcher names the daemon container in
//! [`DAEMON_CONTAINER_ENV`] for the daemon and for the `mj` commands it runs
//! inside that container. Every decision that depends on running there asks
//! this module instead of reading the variable itself.

use crate::worker_launch::HarnessRuntimePolicy;

/// The environment variable naming the daemon container this process runs
/// in. A host daemon and a host `mj` do not have it.
pub const DAEMON_CONTAINER_ENV: &str = "MJ_DAEMON_CONTAINER";

/// The daemon container this process runs in, or `None` on a host.
pub fn daemon_container() -> Option<String> {
    std::env::var(DAEMON_CONTAINER_ENV)
        .ok()
        .filter(|name| !name.trim().is_empty())
}

/// Where harness programs come from for work this machine runs itself: local
/// sessions, the model-discovery probe, `mj login` and local quota checks.
///
/// The daemon container's image carries the pinned harnesses, so they are the
/// only source there. A host gets Mjolnir's managed installation.
pub fn local_harness_runtime() -> HarnessRuntimePolicy {
    if daemon_container().is_some() {
        HarnessRuntimePolicy::Image
    } else {
        HarnessRuntimePolicy::Managed
    }
}
