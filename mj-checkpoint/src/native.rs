//! Native harness storage shared by import and checkpoint restoration.

pub mod muse;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// External Muse sessions use XDG data storage; private mj profiles use their
/// isolated data directory. Configuration credentials are never a session root.
pub fn muse_sessions_root(config_home: &Path) -> Result<PathBuf> {
    let native_config = dirs::config_dir()
        .context("Muse configuration directory is unavailable")?
        .join("muse");
    let native_data = dirs::data_dir()
        .context("Muse data directory is unavailable")?
        .join("muse");
    Ok(muse_sessions_root_at(
        config_home,
        &native_config,
        &native_data,
    ))
}

fn muse_sessions_root_at(config_home: &Path, native_config: &Path, native_data: &Path) -> PathBuf {
    if config_home == native_config {
        native_data.join("sessions")
    } else {
        config_home.join(".data/muse/sessions")
    }
}
