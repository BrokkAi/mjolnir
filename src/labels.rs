//! Stable string labels for ACP protocol enums.
//!
//! Several consumers (the headless `--print` runner and the `mj mcp` server)
//! map the same `agent-client-protocol` enums to machine-readable strings for
//! their JSON output. Those enums are `#[non_exhaustive]`, so each mapping needs
//! a catch-all and must be updated when upstream adds a variant. Keeping the
//! mappers here means there is exactly one place to update rather than several
//! copies that can silently drift apart.

use agent_client_protocol::schema::v1::{
    PermissionOptionKind, StopReason, ToolCallStatus, ToolKind,
};

/// Stable label for a tool-call kind.
pub fn tool_kind_label(kind: ToolKind) -> &'static str {
    match kind {
        ToolKind::Read => "read",
        ToolKind::Edit => "edit",
        ToolKind::Delete => "delete",
        ToolKind::Move => "move",
        ToolKind::Search => "search",
        ToolKind::Execute => "execute",
        ToolKind::Think => "think",
        ToolKind::Fetch => "fetch",
        ToolKind::SwitchMode => "switch_mode",
        ToolKind::Other => "other",
        _ => "other",
    }
}

/// Stable label for a tool-call status.
pub fn tool_status_label(status: ToolCallStatus) -> &'static str {
    match status {
        ToolCallStatus::Pending => "pending",
        ToolCallStatus::InProgress => "in_progress",
        ToolCallStatus::Completed => "completed",
        ToolCallStatus::Failed => "failed",
        _ => "other",
    }
}

/// Stable label for a prompt-turn stop reason.
pub fn stop_reason_label(reason: StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn => "end_turn",
        StopReason::MaxTokens => "max_tokens",
        StopReason::MaxTurnRequests => "max_turn_requests",
        StopReason::Refusal => "refusal",
        StopReason::Cancelled => "cancelled",
        _ => "other",
    }
}

/// Stable label for a permission-option kind.
pub fn permission_option_kind_label(kind: PermissionOptionKind) -> &'static str {
    match kind {
        PermissionOptionKind::AllowOnce => "allow_once",
        PermissionOptionKind::AllowAlways => "allow_always",
        PermissionOptionKind::RejectOnce => "reject_once",
        PermissionOptionKind::RejectAlways => "reject_always",
        _ => "other",
    }
}
