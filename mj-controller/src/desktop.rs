//! Process-boundary protocol shared by `mj` and `mj-desktop`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const DESKTOP_LAUNCH_PROTOCOL_VERSION: u32 = 1;

/// Everything the native desktop process needs from the controller process.
///
/// The signed cookie is a credential. This value must travel through an
/// anonymous pipe, never through command-line arguments or environment
/// variables that process inspection tools commonly expose.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesktopLaunch {
    protocol_version: u32,
    pub viewer_url: String,
    pub bootstrap_cookie_value: String,
}

impl std::fmt::Debug for DesktopLaunch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DesktopLaunch")
            .field("protocol_version", &self.protocol_version)
            .field("viewer_url", &self.viewer_url)
            .field("bootstrap_cookie_value", &"[redacted]")
            .finish()
    }
}

impl DesktopLaunch {
    pub fn new(viewer_url: String, bootstrap_cookie_value: String) -> Self {
        Self {
            protocol_version: DESKTOP_LAUNCH_PROTOCOL_VERSION,
            viewer_url,
            bootstrap_cookie_value,
        }
    }

    pub fn to_json(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(self).context("serialize desktop launch")
    }

    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        let launch: Self = serde_json::from_slice(bytes).context("parse desktop launch")?;
        anyhow::ensure!(
            launch.protocol_version == DESKTOP_LAUNCH_PROTOCOL_VERSION,
            "desktop launch protocol {} is incompatible with supported version {}",
            launch.protocol_version,
            DESKTOP_LAUNCH_PROTOCOL_VERSION
        );
        Ok(launch)
    }
}

/// Resolve a packaged companion beside the currently running executable.
pub fn sibling_executable(current: &Path, basename: &str) -> PathBuf {
    current.with_file_name(format!("{basename}{}", std::env::consts::EXE_SUFFIX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_launch_round_trips_without_debugging_its_cookie() {
        let launch = DesktopLaunch::new(
            "https://localhost:43210/".into(),
            "signed-secret-cookie".into(),
        );

        assert_eq!(
            DesktopLaunch::from_json(&launch.to_json().unwrap()).unwrap(),
            launch
        );
        let debug = format!("{launch:?}");
        assert!(debug.contains("[redacted]"), "{debug}");
        assert!(!debug.contains("signed-secret-cookie"), "{debug}");
    }

    #[test]
    fn desktop_launch_rejects_unknown_fields() {
        let error = DesktopLaunch::from_json(
            br#"{"protocol_version":1,"viewer_url":"https://localhost/","bootstrap_cookie_value":"secret","extra":true}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("parse desktop launch"), "{error}");
        assert!(!error.contains("secret"), "{error}");
    }

    #[test]
    fn desktop_launch_rejects_an_incompatible_protocol() {
        let error = DesktopLaunch::from_json(
            br#"{"protocol_version":99,"viewer_url":"https://localhost/","bootstrap_cookie_value":"secret"}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("protocol 99"), "{error}");
        assert!(!error.contains("secret"), "{error}");
    }

    #[test]
    fn companion_path_replaces_only_the_executable_filename() {
        let current = Path::new("target/profile/mj");
        let expected =
            Path::new("target/profile").join(format!("mj-desktop{}", std::env::consts::EXE_SUFFIX));
        assert_eq!(sibling_executable(current, "mj-desktop"), expected);
    }
}
