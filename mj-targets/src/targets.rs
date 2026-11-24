//! Declarative execution plans for Hel session targets.
//!
//! Plans deliberately contain argv vectors instead of local shell strings.  A
//! shell is used only at the SSH boundary, where OpenSSH necessarily sends a
//! command string; every remotely supplied argument is POSIX-quoted there.

use std::collections::{BTreeMap, BTreeSet};
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

pub mod preflight;
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
mod remote_git;
pub use remote_git::*;
mod session_target;
pub use session_target::*;
mod container_listing;
pub use container_listing::*;
mod managed_target;
pub use managed_target::*;
mod profile_home;
pub use profile_home::*;
mod bootstrap;
pub use bootstrap::*;
mod container;
mod container_upgrade;
mod nitro;
use container::*;
pub use container::{
    CONTAINER_MOUNTS_PREFIX, CreationObservation, creation_observation, inspect_container_storage,
    inspect_container_storage_observed,
};
#[doc(hidden)]
pub use container_upgrade::container_upgrade_test_plan;
pub use container_upgrade::{
    RetainedContainer, inspect_retained_container, recover_removed_container,
    retained_workspace_volume_exists,
};
use nitro::*;
pub use nitro::{
    NitroEngineInfo, NitroEngineUnavailable, NitroMbxRemote, NitroPreflight, nitro_info_command,
    parse_nitro_info, verify_ssh_nitro,
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

/// The target's base image followed by its distinct bundle images for this
/// platform. An unknown platform can only resolve the base image.
pub fn effective_target_images<'a>(
    config: &'a mj_core::config::Config,
    target_id: &str,
    platform: Option<&str>,
) -> Vec<&'a str> {
    let Some(container) = config
        .targets
        .get(target_id)
        .and_then(|target| target.container())
    else {
        return Vec::new();
    };
    let mut images = vec![container.image.as_str()];
    if let Some(platform) = platform {
        for bundle_id in config.bundles.keys() {
            if let Some(image) = config.session_image(Some(bundle_id), target_id, platform)
                && !images.contains(&image)
            {
                images.push(image);
            }
        }
    }
    images
}

#[derive(Debug, Default)]
pub struct ImageRefreshPlan {
    pub pulls: Vec<ImageRefresh>,
    /// Includes images whose pull policy is Never. The host may already have
    /// those images, and their targets still need them.
    pub keep_images: BTreeSet<String>,
    /// The machine's `build_cache.max_total_size`, which also bounds a Nitro
    /// host's proxy cache. Unset means mbx's automatic budget.
    pub mbx_cache_budget: Option<String>,
}

/// Plan downloads, once per (host, image, platform). Targets that share an
/// image take the more eager policy. Supply one host's targets when using
/// the keep set to reconcile its image store.
pub fn image_refresh_plan<'a>(
    config: &mj_core::config::Config,
    target_ids: impl IntoIterator<Item = &'a str>,
    native_platform: Option<&str>,
) -> ImageRefreshPlan {
    let mut plan = ImageRefreshPlan::default();
    for target_id in target_ids {
        let Some((host, container)) = config
            .targets
            .get(target_id)
            .and_then(|target| target.image_host())
        else {
            continue;
        };
        let platform = container.platform.as_deref().or(native_platform);
        if let Some(budget) = container
            .build_cache
            .as_ref()
            .and_then(|cache| cache.max_total_size.clone())
        {
            plan.mbx_cache_budget = Some(budget);
        }
        for image in effective_target_images(config, target_id, platform) {
            plan.keep_images.insert(image.to_owned());
            let Some(refresh) = mj_core::targets::image_refresh(
                host.clone(),
                image,
                platform,
                container.pull_policy,
            ) else {
                continue;
            };
            if let Some(existing) = plan.pulls.iter_mut().find(|entry| {
                entry.host == refresh.host
                    && entry.image == refresh.image
                    && entry.platform == refresh.platform
            }) {
                existing.when = existing.when.max(refresh.when);
            } else {
                plan.pulls.push(refresh);
            }
        }
    }
    plan
}

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
