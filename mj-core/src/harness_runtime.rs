//! Exact harness versions used by Mjolnir-managed installations.
//!
//! Installation and process ownership live in `brokk-mj-worker`; this module
//! contains only shared, inert metadata so the controller, worker, container
//! parity tests, and diagnostics cannot silently disagree about a pin.

use crate::config::HarnessKind;

pub const CODEX_ACP_PACKAGE: &str = "@brokkai/codex-acp";
pub const CODEX_ACP_VERSION: &str = "1.13.3";
pub const CODEX_CLI_VERSION: &str = "0.156.1";
pub const CLAUDE_ACP_VERSION: &str = "0.81.0";
pub const KIMI_VERSION: &str = "2.0.2";
pub const GROK_VERSION: &str = "1.0.40";
pub const MUSE_ACP_VERSION: &str = "0.5.0";
pub const MUSE_VERSION: &str = "1.3.0-R3401.1";

/// The built-in npm launcher, selected on the worker before runtime inspection.
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
            install_id: "muse-acp-0.5.0_muse-1.3.0-R3401.1",
            display_version: "muse-acp 0.5.0 + Muse Code 1.3.0-R3401.1",
            entrypoint: "bin/muse-acp",
        },
        HarnessKind::Codex => HarnessPin {
            install_id: "brokkai-codex-acp-1.13.3_codex-0.156.1",
            display_version: "@brokkai/codex-acp 1.13.3 + codex 0.156.1",
            entrypoint: "node_modules/.bin/codex-acp",
        },
        HarnessKind::Claude => HarnessPin {
            install_id: "claude-agent-acp-0.81.0",
            display_version: "claude-agent-acp 0.81.0",
            entrypoint: "node_modules/.bin/claude-agent-acp",
        },
        HarnessKind::Kimi => HarnessPin {
            install_id: "kimi-2.0.2",
            display_version: "Kimi Code 2.0.2",
            entrypoint: "bin/kimi",
        },
        HarnessKind::Grok => HarnessPin {
            install_id: "grok-1.0.40",
            display_version: "Grok 1.0.40",
            entrypoint: "bin/grok",
        },
    }
}
use serde::{Deserialize, Serialize};

/// Public provenance of a runtime inspected on its executing target.
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

/// Comparison scope excludes model, effort, credentials, homes, and environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeIdentity {
    pub id: Option<String>,
    pub harness: HarnessKind,
    pub platform: String,
    pub provenance: RuntimeProvenance,
    pub components: Vec<RuntimeComponent>,
    pub unavailable_reason: Option<String>,
}

impl RuntimeIdentity {
    pub fn with_reported_agent(
        mut self,
        agent_info: Option<&agent_client_protocol::schema::v1::Implementation>,
    ) -> anyhow::Result<Self> {
        self.components.push(RuntimeComponent {
            name: "acp_reported_agent".into(),
            version: agent_info.map(|info| format!("{} {}", info.name, info.version)),
            sha256: None,
        });
        self.refresh_id()?;
        Ok(self)
    }

    pub fn refresh_id(&mut self) -> anyhow::Result<()> {
        use sha2::Digest;
        self.components.sort_by(|a, b| a.name.cmp(&b.name));
        self.id = if self.unavailable_reason.is_some() {
            None
        } else {
            let body = serde_json::to_vec(&(
                1,
                self.harness,
                &self.platform,
                self.provenance,
                &self.components,
            ))?;
            Some(format!(
                "mj-runtime-v1:{}",
                crate::hex::lower_hex(sha2::Sha256::digest(body))
            ))
        };
        Ok(())
    }

    pub fn require(&self, expected: &str) -> anyhow::Result<()> {
        match self.id.as_deref() {
            Some(actual) if actual == expected => Ok(()),
            Some(actual) => anyhow::bail!(
                "runtime identity mismatch: expected {expected}, resolved {actual}; discover the current runtime and explicitly update the selection"
            ),
            None => anyhow::bail!(
                "runtime identity unavailable: {}; select a runtime with known provenance before requiring an identity",
                self.unavailable_reason
                    .as_deref()
                    .unwrap_or("target runtime could not be identified")
            ),
        }
    }
}

/// One immutable initialization, identified by its position in the worker journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeReceipt {
    #[serde(flatten)]
    pub identity: RuntimeIdentity,
    pub event_ordinal: u64,
    pub observed_at_ms: i64,
}

pub fn validate_expected_identity(identity: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !identity.is_empty()
            && identity.len() <= 256
            && identity.bytes().all(|byte| byte.is_ascii_graphic()),
        "expected runtime identity must be a nonempty comparison ID of at most 256 ASCII characters"
    );
    Ok(())
}
