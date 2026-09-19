//! Exact harness versions used by Mjolnir-managed bare workers.
//!
//! Installation and process ownership live in `brokk-mj-worker`; this module
//! contains only shared, inert metadata so the controller, worker, container
//! parity tests, and diagnostics cannot silently disagree about a pin.

use crate::config::HarnessKind;

pub const CODEX_ACP_PACKAGE: &str = "@brokkai/codex-acp";
pub const CODEX_ACP_VERSION: &str = "1.11.4";
pub const CODEX_CLI_VERSION: &str = "0.155.1";
pub const CLAUDE_ACP_VERSION: &str = "0.79.0";
pub const KIMI_VERSION: &str = "2.0.2";
pub const GROK_VERSION: &str = "1.0.34";
pub const MUSE_ACP_VERSION: &str = "0.4.5";
pub const MUSE_VERSION: &str = "1.3.0-R3401.1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HarnessPin {
    pub install_id: &'static str,
    pub display_version: &'static str,
    pub entrypoint: &'static str,
}

pub const fn pin(kind: HarnessKind) -> HarnessPin {
    match kind {
        HarnessKind::Muse => HarnessPin {
            install_id: "muse-acp-0.4.5_muse-1.3.0-R3401.1",
            display_version: "muse-acp 0.4.5 + Muse Code 1.3.0-R3401.1",
            entrypoint: "bin/muse-acp",
        },
        HarnessKind::Codex => HarnessPin {
            install_id: "brokkai-codex-acp-1.11.4_codex-0.155.1",
            display_version: "@brokkai/codex-acp 1.11.4 + codex 0.155.1",
            entrypoint: "node_modules/.bin/codex-acp",
        },
        HarnessKind::Claude => HarnessPin {
            install_id: "claude-agent-acp-0.79.0",
            display_version: "claude-agent-acp 0.79.0",
            entrypoint: "node_modules/.bin/claude-agent-acp",
        },
        HarnessKind::Kimi => HarnessPin {
            install_id: "kimi-2.0.2",
            display_version: "Kimi Code 2.0.2",
            entrypoint: "bin/kimi",
        },
        HarnessKind::Grok => HarnessPin {
            install_id: "grok-1.0.34",
            display_version: "Grok 1.0.34",
            entrypoint: "bin/grok",
        },
    }
}
