//! Native harness storage shared by import and checkpoint restoration.

pub mod muse;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// External Muse sessions use XDG data storage; private mj profiles use their
/// isolated data directory. Configuration credentials are never a session root.
pub fn muse_sessions_root(config_home: &Path) -> Result<PathBuf> {
    let data = mj_core::config::HarnessKind::Muse
        .nested_data_dir(config_home)
        .context("Muse data directory is unavailable")?;
    Ok(data.join("sessions"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hard-won: fab6694e7: platform-native dirs sent Muse imports outside XDG on macOS and Windows.
    #[test]
    fn muse_external_and_private_profiles_resolve_separate_data_roots() {
        let home = mj_core::path_input::user_home().unwrap();
        // Muse follows XDG on macOS and Windows too, where dirs::config_dir
        // and dirs::data_dir select a different native application directory.
        let xdg = |name: &str, fallback: &str| {
            std::env::var_os(name)
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .unwrap_or_else(|| home.join(fallback))
                .join("muse")
        };
        let native_config = xdg("XDG_CONFIG_HOME", ".config");
        let native_data = xdg("XDG_DATA_HOME", ".local/share");
        assert_eq!(
            muse_sessions_root(&native_config).unwrap(),
            native_data.join("sessions")
        );
        let directory = tempfile::tempdir().unwrap();
        let private = directory.path().join("muse");
        assert_eq!(
            muse_sessions_root(&private).unwrap(),
            private.join(".data/muse/sessions")
        );
    }
}
