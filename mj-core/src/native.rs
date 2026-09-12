//! Native harness storage shared by import and checkpoint restoration.

pub mod deepseek;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn muse_external_and_private_profiles_resolve_separate_data_roots() {
        let native_config = Path::new("/xdg/config/muse");
        let native_data = Path::new("/xdg/data/muse");
        assert_eq!(
            muse_sessions_root_at(native_config, native_config, native_data),
            native_data.join("sessions")
        );
        let private = Path::new("/profiles/account/muse");
        assert_eq!(
            muse_sessions_root_at(private, native_config, native_data),
            private.join(".data/muse/sessions")
        );
    }
}
