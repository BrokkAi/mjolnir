//! Hel's target-side worker: the daemon and stdio proxy that run inside a
//! container or on an SSH host.

/// Default diagnostics include the local Jev and Mjolnir guardian decision
/// trails. Explicit RUST_LOG overrides it.
pub const DEFAULT_WORKER_LOG_FILTER: &str = "warn,mj_jev=info,mj_guardian=info";

pub mod user_shell;
pub mod worker_runtime;
pub mod worker_stop;

pub mod relay;

pub mod acp;
pub mod authorized_keys;
pub mod exit_record;
pub mod terminal;

pub mod checkpoint;

pub mod mailbox_hook;
mod mcp_stdio;
pub mod memory_mcp;
// Firecracker, vsock, TAP devices and PID 1 duties exist only on Linux.
#[cfg(target_os = "linux")]
pub mod microvm;
pub mod review;
pub mod subagent_mcp;

#[cfg(all(test, unix))]
mod checkpoint_tests;
#[cfg(test)]
mod test_support;
// Served by the controller and daemon test binaries; see the module.
#[doc(hidden)]
pub mod stand_in_relay;

pub mod cpu_usage;
pub mod work_facts;
