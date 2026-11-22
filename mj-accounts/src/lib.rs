//! Accounts for Mjolnir's harness profiles: whether a profile is logged in,
//! the terminal a login runs in, and each provider's quota and usage reader
//! with the scheduling and health derived from them. It builds on the targets
//! and store crates and knows nothing of sessions.
//!
//! This is a separate crate so that the session runtime, the lifecycle
//! operations and the daemon can each use it without rebuilding one another.

#[cfg(test)]
mj_core::enable_test_fixtures_on_startup!();

// The moved modules name these by their old paths.
pub(crate) use mj_store::{database, upgrade};
pub(crate) use mj_targets::{streamed_child, targets};
#[cfg(test)]
pub(crate) use mj_targets::{test_isolation, test_log};

pub mod antigravity_usage;
pub mod claude_login;
pub mod claude_usage;
pub mod codex_usage;
pub mod grok_usage;
pub mod harness_auth;
pub mod harness_login;
pub mod kimi_auth;
pub mod login_terminal;
pub mod muse_usage;
pub mod opencode_logins;
pub mod power_ranking;
pub mod profile_health;
pub mod profile_login;
pub mod profile_scheduler;
pub mod quota;
mod quota_harness;
pub mod quota_telemetry;
pub mod review_selection;
pub mod zai_usage;
