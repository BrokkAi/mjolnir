//! Selective utility capabilities and the worker-local JSON-line protocol.
use crate::config::HarnessKind;
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const CLAUDE_MCP_SERVER: &str = "mj-claude";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeMcpCapability {
    WebFetch,
    Monitor,
    Cron,
    ScheduleWakeup,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeMcpCapabilities {
    capabilities: &'static [ClaudeMcpCapability],
}

impl ClaudeMcpCapabilities {
    pub fn for_harness(harness: HarnessKind) -> Self {
        use ClaudeMcpCapability::*;
        Self {
            capabilities: match harness {
                HarnessKind::Claude => &[],
                HarnessKind::Codex => &[WebFetch, Monitor, Cron, ScheduleWakeup],
                HarnessKind::Antigravity => &[WebFetch, Monitor],
                HarnessKind::OpenCode | HarnessKind::Dsh | HarnessKind::Grok => {
                    &[Monitor, Cron, ScheduleWakeup]
                }
                HarnessKind::Kimi => &[Monitor, ScheduleWakeup],
                HarnessKind::Muse => &[ScheduleWakeup],
            },
        }
    }
    pub fn is_empty(&self) -> bool {
        self.capabilities.is_empty()
    }
    pub fn contains(&self, capability: ClaudeMcpCapability) -> bool {
        self.capabilities.contains(&capability)
    }
    pub fn capabilities(&self) -> &'static [ClaudeMcpCapability] {
        self.capabilities
    }
    pub fn tool_names(&self) -> Vec<&'static str> {
        self.capabilities
            .iter()
            .flat_map(|capability| match capability {
                ClaudeMcpCapability::WebFetch => &["web_fetch"][..],
                ClaudeMcpCapability::Monitor => &["monitor", "monitor_stop", "monitor_list"][..],
                ClaudeMcpCapability::Cron => &["cron_create", "cron_list", "cron_delete"][..],
                ClaudeMcpCapability::ScheduleWakeup => &["schedule_wakeup"][..],
            })
            .copied()
            .collect()
    }
    pub fn supports_tool(&self, name: &str) -> bool {
        self.tool_names().contains(&name)
    }
    pub fn server_args(socket: &Path, harness: HarnessKind) -> Vec<String> {
        [
            "worker",
            "claude-mcp",
            "--socket",
            &socket.to_string_lossy(),
            "--harness",
            harness.id(),
        ]
        .map(str::to_owned)
        .into()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeMcpRpcRequest {
    pub tool: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeMcpRpcResponse {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
