//! Exact harness versions used by Mjolnir-managed installations.
//!
//! Installation and process ownership live in `brokk-mj-worker`; this module
//! contains only shared, inert metadata so the controller, worker, container
//! parity tests, and diagnostics cannot silently disagree about a pin.

use crate::config::HarnessKind;

pub const CODEX_ACP_PACKAGE: &str = "@brokkai/codex-acp";
pub const CODEX_ACP_VERSION: &str = "1.13.6";
pub const CODEX_CLI_VERSION: &str = "0.160.1";
pub const CLAUDE_ACP_VERSION: &str = "0.87.0";
pub const CLAUDE_CLI_VERSION: &str = "2.1.293";
pub const KIMI_VERSION: &str = "2.1.1";
pub const GROK_VERSION: &str = "1.0.40";
pub const MUSE_ACP_VERSION: &str = "0.10.0";
pub const MUSE_VERSION: &str = "1.4.2-R4684.1";
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
            install_id: "muse-acp-0.10.0_muse-1.4.2-R4684.1",
            display_version: "muse-acp 0.10.0 + Muse Code 1.4.2-R4684.1",
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
use serde::{Deserialize, Serialize};

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
