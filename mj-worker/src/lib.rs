//! Hel's target-side worker: the daemon and stdio proxy that run inside a
//! container or on an SSH host.

/// Default diagnostics include the local Jev decision trail. Explicit RUST_LOG overrides it.
pub const DEFAULT_WORKER_LOG_FILTER: &str = "warn,mj_jev=info";

pub mod user_shell;
pub mod worker_runtime;

pub mod relay;

pub mod acp;
pub mod terminal;

pub mod checkpoint;

mod mcp_stdio;
pub mod memory_mcp;
pub mod review;
pub mod subagent_mcp;

#[cfg(all(test, unix))]
mod checkpoint_tests;
#[cfg(test)]
mod test_support;
