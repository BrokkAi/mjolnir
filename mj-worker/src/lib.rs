//! Hel's target-side worker: the daemon and stdio proxy that run inside a
//! container or on an SSH host.

pub mod user_shell;
pub mod worker_runtime;

pub mod relay;

pub mod acp;
pub mod terminal;

pub mod checkpoint;

pub mod memory_mcp;
pub mod review;

#[cfg(all(test, unix))]
mod checkpoint_tests;
