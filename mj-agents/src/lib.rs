//! Multi-agent orchestration, review, and workspace product layer.

pub mod discrete_review;
pub mod live;
pub mod quota;
pub mod subagent;

pub use mj_core::{deepswe, pull_request, trajectory, workspace_snapshot, worktree};
