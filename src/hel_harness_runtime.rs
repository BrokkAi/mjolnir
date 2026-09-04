//! Exact harness versions used by Mjolnir-managed remote workers.
//!
//! Installation and process ownership live in `brokk-mj-worker`; this module
//! contains only shared, inert metadata so the controller, worker, container
//! parity tests, and diagnostics cannot silently disagree about a pin.

use crate::hel_config::HarnessKind;

pub const CODEX_ACP_VERSION: &str = "1.8.0";
pub const CODEX_CLI_VERSION: &str = "0.151.0";
pub const CLAUDE_ACP_VERSION: &str = "0.73.0";
pub const KIMI_VERSION: &str = "0.41.0";
pub const GROK_VERSION: &str = "1.0.13";
pub const DEEPSEEK_DSH_VERSION: &str = "0.1.1-rc.2";
pub const DEEPSEEK_ACP_VERSION: &str = "0.10.0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HarnessPin {
    pub install_id: &'static str,
    pub display_version: &'static str,
    pub entrypoint: &'static str,
}

pub const fn pin(kind: HarnessKind) -> HarnessPin {
    match kind {
        HarnessKind::Codex => HarnessPin {
            install_id: "codex-acp-1.8.0_codex-0.151.0",
            display_version: "codex-acp 1.8.0 + codex 0.151.0",
            entrypoint: "node_modules/.bin/codex-acp",
        },
        HarnessKind::Claude => HarnessPin {
            install_id: "claude-agent-acp-0.73.0",
            display_version: "claude-agent-acp 0.73.0",
            entrypoint: "node_modules/.bin/claude-agent-acp",
        },
        HarnessKind::Kimi => HarnessPin {
            install_id: "kimi-0.41.0",
            display_version: "Kimi Code 0.41.0",
            entrypoint: "bin/kimi",
        },
        HarnessKind::Grok => HarnessPin {
            install_id: "grok-1.0.13",
            display_version: "Grok 1.0.13",
            entrypoint: "bin/grok",
        },
        HarnessKind::Deepseek => HarnessPin {
            install_id: "dsh-0.1.1-rc.2_acp-0.10.0",
            display_version: "dsh 0.1.1-rc.2 + dsh-acp-server 0.10.0",
            entrypoint: "node_modules/.bin/dsh-acp-server",
        },
    }
}
