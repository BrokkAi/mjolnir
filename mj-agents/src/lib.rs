//! Multi-agent orchestration, review, Ragnarok, and workspace product layer.

pub mod discrete_review;
pub mod quota;
pub mod ragnarok;
pub mod ragnarok_sprites;
pub mod subagent;

pub use mj_core::{deepswe, pull_request, trajectory, workspace_snapshot, worktree};
