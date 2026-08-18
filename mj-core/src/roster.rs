//! Frontend-neutral model and adapter roster types.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::{PermissionPreset, RuntimePermissionConfig};

/// One ranked model from the external quality catalog.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ModelRow {
    pub model: String,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    pub pass_at_1: f64,
    pub mean_cost_usd: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterKind {
    Codex,
    Claude,
}

impl AdapterKind {
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude Code",
        }
    }

    pub fn from_source_id(source_id: &str) -> Option<Self> {
        match source_id {
            "codex-acp" => Some(Self::Codex),
            "claude-acp" => Some(Self::Claude),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AdapterLaunch {
    pub kind: AdapterKind,
    pub source_id: String,
    pub command: PathBuf,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
}

pub fn configure_permissions(
    kind: AdapterKind,
    mode: PermissionPreset,
    _env: &mut HashMap<String, String>,
) -> Option<RuntimePermissionConfig> {
    let (config_id, value, manual_fallback) = match (kind, mode) {
        (AdapterKind::Codex, PermissionPreset::Manual) => ("mode", "read-only", None),
        (AdapterKind::Codex, PermissionPreset::Auto) => ("mode", "agent", Some("read-only")),
        (AdapterKind::Codex, PermissionPreset::Yolo) => ("mode", "agent-full-access", None),
        (AdapterKind::Claude, PermissionPreset::Manual) => ("mode", "default", None),
        (AdapterKind::Claude, PermissionPreset::Auto) => ("mode", "auto", Some("default")),
        (AdapterKind::Claude, PermissionPreset::Yolo) => ("mode", "bypassPermissions", None),
    };
    Some(RuntimePermissionConfig {
        config_id: config_id.to_string(),
        value: value.to_string(),
        manual_fallback: manual_fallback.map(str::to_string),
        mode,
    })
}

#[derive(Debug, Clone)]
pub struct ResolvedAgent {
    pub model: ModelRow,
    pub model_value: String,
    pub launch: AdapterLaunch,
    pub ranked: bool,
    pub reasoning_effort: Option<String>,
}

/// A signed-in subscription and its relative monthly capacity.
#[derive(Debug, Clone, PartialEq)]
pub struct Subscription {
    pub label: String,
    pub capacity: f64,
}

impl Subscription {
    pub fn new(label: impl Into<String>, capacity: f64) -> Self {
        Self {
            label: label.into(),
            capacity,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Subscriptions {
    pub claude: Option<Subscription>,
    pub codex: Option<Subscription>,
}

impl Subscriptions {
    pub fn for_adapter(&self, kind: AdapterKind) -> Option<&Subscription> {
        match kind {
            AdapterKind::Claude => self.claude.as_ref(),
            AdapterKind::Codex => self.codex.as_ref(),
        }
    }

    pub fn favored(&self) -> Option<AdapterKind> {
        let claude = self.claude.as_ref()?;
        let codex = self.codex.as_ref()?;
        match claude.capacity.total_cmp(&codex.capacity) {
            std::cmp::Ordering::Greater => Some(AdapterKind::Claude),
            std::cmp::Ordering::Less => Some(AdapterKind::Codex),
            std::cmp::Ordering::Equal => None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct AcpInventory {
    pub servers: Vec<AcpServerInfo>,
}

#[derive(Debug, Clone)]
pub struct AcpServerInfo {
    pub id: String,
    pub label: String,
    pub policy: crate::config::AcpServerPolicy,
    pub detected: bool,
    pub selected: bool,
    pub evidence: String,
    pub launch: AdapterLaunch,
    pub model_count: usize,
    pub error: Option<String>,
    pub session_config: Vec<agent_client_protocol::schema::v1::SessionConfigOption>,
    pub subscription: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ModelChoice {
    pub model: String,
    pub pass_at_1: f64,
    pub mean_cost_usd: f64,
    pub available: bool,
    pub disabled_reason: Option<String>,
    pub adapter: Option<String>,
    pub ranked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeAuthStatus {
    LoggedIn,
    NotLoggedIn,
}

impl ClaudeAuthStatus {
    pub fn logged_in(self) -> bool {
        self == Self::LoggedIn
    }

    pub fn unavailable_reason(self) -> &'static str {
        match self {
            Self::LoggedIn => "Claude Code is logged in",
            Self::NotLoggedIn => "Claude credentials not found",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Availability {
    pub codex_credentials: bool,
    pub claude_status: ClaudeAuthStatus,
    pub subscriptions: Subscriptions,
}

impl Availability {
    pub fn missing_reason(&self, model: &str) -> Option<&'static str> {
        let adapter = match crate::model_resolve::model_provider(model) {
            "openai" => Some(AdapterKind::Codex),
            "anthropic" => Some(AdapterKind::Claude),
            _ => None,
        }?;
        match adapter {
            AdapterKind::Codex if !self.codex_credentials => Some("Codex credentials not found"),
            AdapterKind::Claude if !self.claude_status.logged_in() => {
                Some(self.claude_status.unavailable_reason())
            }
            _ => None,
        }
    }
}

/// Roles and catalog state bound by one roster resolution pass.
#[derive(Debug, Clone)]
pub struct Roster {
    pub primary: ResolvedAgent,
    pub review_supervisor: Option<ResolvedAgent>,
    pub subagent_default: Option<ResolvedAgent>,
    pub available: Vec<ResolvedAgent>,
    pub choices: Vec<ModelChoice>,
    pub warnings: Vec<String>,
    pub inventory: AcpInventory,
    pub subagent_acp_priority: Vec<String>,
    pub subagent_acp_source: Option<String>,
}
