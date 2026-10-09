//! Exact harness versions used by Mjolnir-managed installations.
//!
//! Installation and process ownership live in `brokk-mj-worker`; this module
//! holds shared pin metadata and managed-install paths/manifests so the
//! controller and worker cannot silently disagree about an installation.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::HarnessKind;

pub const MANIFEST_FILE: &str = "mj-harness.json";
pub const LEASE_FILE: &str = ".lease";
pub const CODEX_CLI_ENTRYPOINT: &str = "node_modules/.bin/codex";
const MANAGED_HARNESSES_DIR: &str = "mjolnir/harnesses";

/// The install receipt written by the worker and checked by managed clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedHarnessManifest {
    schema: u32,
    harness: HarnessKind,
    install_id: String,
}

impl ManagedHarnessManifest {
    pub fn for_harness(harness: HarnessKind) -> Self {
        Self::for_install(harness, pin(harness).install_id)
    }

    pub fn for_install(harness: HarnessKind, install_id: &str) -> Self {
        Self {
            schema: 1,
            harness,
            install_id: install_id.to_owned(),
        }
    }
}

/// Resolve the shared managed-harness cache root from the target environment.
///
/// A non-empty `XDG_CACHE_HOME` takes precedence over `HOME/.cache`; the
/// resulting base must be absolute on every target.
pub fn managed_harness_cache_root(
    xdg_cache_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<PathBuf> {
    let base = match xdg_cache_home.filter(|path| !path.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(
            home.filter(|path| !path.is_empty())
                .context("managed harness installation needs HOME or XDG_CACHE_HOME")?,
        )
        .join(".cache"),
    };
    if !base.is_absolute() {
        bail!(
            "managed harness cache root must be absolute: {}",
            base.display()
        );
    }
    Ok(base.join(MANAGED_HARNESSES_DIR))
}

/// Resolve the pinned install directory under a managed-harness cache root.
pub fn managed_harness_install_dir(cache_root: &Path, harness: HarnessKind) -> PathBuf {
    cache_root.join(harness.id()).join(pin(harness).install_id)
}

/// Check that an installation carries the receipt for the current harness pin.
/// Missing and malformed receipts are incomplete installs; other read failures
/// are returned with their path for diagnostics.
pub fn managed_harness_manifest_matches(
    path: &Path,
    harness: HarnessKind,
    install_id: &str,
) -> Result<bool> {
    let manifest_path = path.join(MANIFEST_FILE);
    let body = match std::fs::read(&manifest_path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("read managed harness manifest {}", manifest_path.display())
            });
        }
    };
    let manifest: ManagedHarnessManifest = match serde_json::from_slice(&body) {
        Ok(manifest) => manifest,
        Err(_) => return Ok(false),
    };
    Ok(manifest == ManagedHarnessManifest::for_install(harness, install_id))
}

pub const CODEX_ACP_PACKAGE: &str = "@brokkai/codex-acp";
pub const CODEX_ACP_VERSION: &str = "1.13.6";
pub const CODEX_CLI_VERSION: &str = "0.160.1";
pub const CLAUDE_ACP_VERSION: &str = "0.87.0";
pub const CLAUDE_CLI_VERSION: &str = "2.1.293";
pub const KIMI_VERSION: &str = "2.1.1";
pub const GROK_VERSION: &str = "1.0.40";
pub const MUSE_ACP_VERSION: &str = "0.11.0";
pub const MUSE_VERSION: &str = "1.4.4-R5419.1";
pub const OPENCODE_VERSION: &str = "1.18.34";

/// The built-in npm launcher, selected on the worker before ACP startup.
#[derive(Clone, Copy)]
pub struct NpmBridge {
    pub command: &'static str,
    pub package: &'static str,
    pub version: &'static str,
}

pub const fn npm_bridge(kind: HarnessKind) -> Option<NpmBridge> {
    match kind {
        HarnessKind::Codex => Some(NpmBridge {
            command: "codex-acp",
            package: CODEX_ACP_PACKAGE,
            version: CODEX_ACP_VERSION,
        }),
        HarnessKind::Claude => Some(NpmBridge {
            command: "claude-agent-acp",
            package: "@agentclientprotocol/claude-agent-acp",
            version: CLAUDE_ACP_VERSION,
        }),
        _ => None,
    }
}

impl NpmBridge {
    pub fn matches_launcher(&self, command: &std::path::Path, args: &[String]) -> bool {
        (command == std::path::Path::new(self.command) && args.is_empty())
            || self.is_legacy_launcher(command, args)
    }

    /// Keep launch descriptions usable by older workers, including reviewers on
    /// a busy worker that cannot upgrade yet. New workers resolve this before ACP.
    pub fn bootstrap_script(&self) -> String {
        self.script_for_version(self.version)
    }

    /// Recognize persisted launch descriptions from before worker-side selection.
    /// Match the entire generated script; arbitrary shell wrappers stay custom.
    pub fn is_legacy_launcher(&self, command: &std::path::Path, args: &[String]) -> bool {
        if command != std::path::Path::new("sh") || args.len() != 2 || args[0] != "-c" {
            return false;
        }
        let Some((_, version)) = args[1].rsplit_once(&format!("exec npx -y {}@", self.package))
        else {
            return false;
        };
        if version.is_empty()
            || !version
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b".-+".contains(&byte))
        {
            return false;
        }
        args[1] == self.script_for_version(version)
    }

    fn script_for_version(&self, version: &str) -> String {
        let check = if self.package == CODEX_ACP_PACKAGE {
            format!(
                " && [ \"$(codex-acp --version 2>/dev/null)\" = \"{} {version}\" ]",
                self.package
            )
        } else {
            String::new()
        };
        let node_check = "if ! command -v node >/dev/null 2>&1 || ! command -v npm >/dev/null 2>&1 || ! command -v npx >/dev/null 2>&1; then echo 'Mjolnir needs Node.js, npm, and npx on PATH; install Node in the target environment' >&2; exit 127; fi";
        format!(
            "if command -v {command} >/dev/null 2>&1{check}; then exec {command}; fi; {node_check}; exec npx -y {package}@{version}",
            command = self.command,
            package = self.package,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HarnessPin {
    pub install_id: &'static str,
    pub display_version: &'static str,
    pub entrypoint: &'static str,
}

pub const fn pin(kind: HarnessKind) -> HarnessPin {
    match kind {
        HarnessKind::Muse => HarnessPin {
            install_id: "muse-acp-0.11.0_muse-1.4.4-R5419.1",
            display_version: "muse-acp 0.11.0 + Muse Code 1.4.4-R5419.1",
            entrypoint: "bin/muse-acp",
        },
        HarnessKind::Codex => HarnessPin {
            install_id: "brokkai-codex-acp-1.13.6_codex-0.160.1",
            display_version: "@brokkai/codex-acp 1.13.6 + codex 0.160.1",
            entrypoint: "node_modules/.bin/codex-acp",
        },
        HarnessKind::Claude => HarnessPin {
            install_id: "claude-agent-acp-0.87.0_claude-2.1.293",
            display_version: "claude-agent-acp 0.87.0 + Claude Code 2.1.293",
            entrypoint: "node_modules/.bin/claude-agent-acp",
        },
        HarnessKind::Kimi => HarnessPin {
            install_id: "kimi-2.1.1",
            display_version: "Kimi Code 2.1.1",
            entrypoint: "bin/kimi",
        },
        HarnessKind::Grok => HarnessPin {
            install_id: "grok-1.0.40",
            display_version: "Grok 1.0.40",
            entrypoint: "bin/grok",
        },
        HarnessKind::OpenCode => HarnessPin {
            install_id: "opencode-1.18.34",
            display_version: "OpenCode 1.18.34",
            entrypoint: "opencode",
        },
    }
}
/// Legacy receipt shapes retained to decode journals and events from shipped releases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeProvenance {
    ManagedInstallation,
    TargetInstallation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeComponent {
    pub name: String,
    pub version: Option<String>,
    pub sha256: Option<String>,
}

/// Historical runtime identity; new workers no longer inspect installations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeIdentity {
    pub id: Option<String>,
    pub harness: HarnessKind,
    pub platform: String,
    pub provenance: RuntimeProvenance,
    pub components: Vec<RuntimeComponent>,
    pub unavailable_reason: Option<String>,
}

/// One immutable initialization, identified by its position in the worker journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeReceipt {
    #[serde(flatten)]
    pub identity: RuntimeIdentity,
    pub event_ordinal: u64,
    pub observed_at_ms: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_cache_root_prefers_xdg_and_falls_back_when_it_is_empty() {
        // `temp_dir` is absolute on every target; a hardcoded Unix-style
        // `/custom-cache` is relative on Windows and trips the absolute-path
        // guard the function promises on all platforms.
        let xdg = std::env::temp_dir().join("mj-xdg-cache");
        let home = std::env::temp_dir().join("mj-home");
        let xdg_root = managed_harness_cache_root(Some(xdg.as_os_str()), Some(home.as_os_str()))
            .expect("absolute XDG cache root");
        assert_eq!(xdg_root, xdg.join(MANAGED_HARNESSES_DIR));

        let home_root = managed_harness_cache_root(Some(OsStr::new("")), Some(home.as_os_str()))
            .expect("HOME fallback");
        assert_eq!(home_root, home.join(".cache").join(MANAGED_HARNESSES_DIR));

        assert!(
            managed_harness_cache_root(Some(OsStr::new("relative-cache")), Some(home.as_os_str()))
                .is_err(),
            "a relative XDG cache root stays rejected on every target"
        );
    }

    #[test]
    fn managed_manifest_rejects_a_different_install_id() {
        let temp = tempfile::tempdir().expect("tempdir");
        let install = managed_harness_install_dir(temp.path(), HarnessKind::Codex);
        std::fs::create_dir_all(&install).expect("create install");
        assert!(
            !managed_harness_manifest_matches(
                &install,
                HarnessKind::Codex,
                pin(HarnessKind::Codex).install_id
            )
            .expect("missing manifest")
        );

        let manifest = ManagedHarnessManifest::for_harness(HarnessKind::Codex);
        std::fs::write(
            install.join(MANIFEST_FILE),
            serde_json::to_vec(&manifest).expect("serialize manifest"),
        )
        .expect("write manifest");
        assert!(
            managed_harness_manifest_matches(
                &install,
                HarnessKind::Codex,
                pin(HarnessKind::Codex).install_id
            )
            .expect("matching manifest")
        );
        assert!(
            !managed_harness_manifest_matches(&install, HarnessKind::Codex, "other-install")
                .expect("different install id")
        );
    }
}
