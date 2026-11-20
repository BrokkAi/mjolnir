//! Shared transcript interpretation, projection and presentation.

pub mod handoff;
pub mod projection;
pub mod transcript;

pub mod summary;
pub mod turn_context;

pub mod authorization;

pub mod github_item;

pub mod github_history;

/// Stable transcript item identity for a raw ACP tool call ID.
pub fn tool_item_id(call_id: impl std::fmt::Display) -> String {
    format!("tool:{call_id}")
}
