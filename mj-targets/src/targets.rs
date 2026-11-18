//! Declarative execution plans for Hel session targets.
//!
//! Plans deliberately contain argv vectors instead of local shell strings.  A
//! shell is used only at the SSH boundary, where OpenSSH necessarily sends a
//! command string; every remotely supplied argument is POSIX-quoted there.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

use mj_core::config::ImagePullPolicy;

pub use mj_core::targets::*;
/// Published copies of `docs/PODMAN.md` and `docs/DOCKER.md`
/// (`docs/scripts/sync-podman.mjs` copies them into the site at
/// `docs/astro.config.mjs`'s `site`). Messages link these because an
/// installed user has no repository checkout.
pub const PODMAN_DOCUMENTATION_URL: &str = "https://mjolnir.brokk.ai/podman/";
pub const DOCKER_DOCUMENTATION_URL: &str = "https://mjolnir.brokk.ai/docker/";

// `mj doctor` prints a self-contained setup page that quotes these two pages in
// full. They are embedded here, beside the paths that name them, because this
// crate's `include` list is what carries `docs/` into the published package;
// the controller crate that renders the page cannot reach outside its own
// directory.
/// The rootless Podman postconditions page, verbatim.
pub const PODMAN_DOCUMENTATION: &str = include_str!("../docs/PODMAN.md");
/// The Docker postconditions page, verbatim.
pub const DOCKER_DOCUMENTATION: &str = include_str!("../docs/DOCKER.md");

const PODMAN_MINIMUM_VERSION: (u32, u32) = (4, 0);

mod preflight;
pub use preflight::*;
mod platform;
pub use platform::*;
mod provision;
pub use provision::provision_local_bundle_plan;
pub use provision::*;
mod recovery;
pub use recovery::*;
mod resources;
pub use resources::*;
mod worker_daemon;
pub use worker_daemon::*;
mod cleanup;
pub use cleanup::*;
mod bootstrap;
pub use bootstrap::*;
mod container;
mod container_upgrade;
mod nitro;
pub use container::inspect_container_storage;
use container::*;
#[cfg(any(test, feature = "test-support"))]
pub use container_upgrade::container_upgrade_test_plan;
pub use container_upgrade::{
    RetainedContainer, inspect_retained_container, recover_removed_container,
    retained_workspace_volume_exists,
};
use nitro::*;
pub use nitro::{
    NitroEngineInfo, NitroEngineUnavailable, NitroPreflight, nitro_info_command, parse_nitro_info,
    verify_ssh_nitro,
};
mod process_limit;
pub use process_limit::*;

/// Resolve the primary repository's directory on a target. Session indexing
/// uses the same rule as API file exports so both surfaces name the agent's
/// actual working directory.
pub fn agent_working_directory_at(workspace_root: &str, destination: &Path) -> String {
    target_join(workspace_root, destination)
}

/// Join a relative path onto a target-side root.
///
/// Target paths are POSIX text whatever this daemon runs on, so the components
/// are spelled with `/` here rather than by the host's separator.
pub fn target_join(root: &str, relative: &Path) -> String {
    let mut path = root.trim_end_matches('/').to_owned();
    for component in relative.components() {
        if let std::path::Component::Normal(part) = component {
            path.push('/');
            path.push_str(&part.to_string_lossy());
        }
    }
    path
}

#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
/// The command an SSH remote line runs, without the macOS system PATH prefix
/// every remote line carries.
pub fn unwrapped_remote_line(remote: &str) -> &str {
    let prefix = format!("{} ", mj_core::targets::remote_command_line(&[]));
    remote
        .strip_prefix(prefix.as_str())
        .unwrap_or_else(|| panic!("{remote:?} does not run under the macOS system PATH"))
}

#[cfg(test)]
mod tests;
