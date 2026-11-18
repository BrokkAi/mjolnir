//! Session targets for Mjolnir: where a session's worker runs (a local
//! directory, a container, an SSH host, an EC2 instance) and the commands that
//! provision, control and clean it up. Nothing here knows about the store or
//! about sessions' lifecycle; the crates above compose these plans.
//!
//! This is the lowest crate of the daemon side, so the store, the session
//! runtime and the lifecycle operations can each build on it without building
//! on each other.

pub mod aws_assume_role;
pub mod aws_credentials;
pub mod command_checks;
pub mod image_pull_gate;
pub mod signing_key;
pub mod streamed_child;
pub mod tailscale;
pub mod target_storage;
pub mod targets;
pub mod termination;
#[cfg(any(test, feature = "test-support"))]
pub mod test_isolation;
#[cfg(any(test, feature = "test-support"))]
pub mod test_log;
