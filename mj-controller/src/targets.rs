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

/// `--userns=keep-id:uid=<uid>,gid=<gid>`, which maps a session container's image user
/// onto the host user, landed in Podman 4.3.0.
const PODMAN_MINIMUM_VERSION: (u32, u32) = (4, 3);

mod preflight;
pub use preflight::*;
mod provision;
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
use container::*;
mod process_limit;
pub use process_limit::*;

#[cfg(test)]
mod tests;
