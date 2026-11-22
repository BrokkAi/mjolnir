//! The Mjolnir daemon's store: the SQLite schema and its migrations, the
//! session, environment and event records, the single database writer, and
//! the gates that order recovery and worker lifecycle against it. It builds on
//! `mj-targets` for the target types a record holds and knows nothing of how
//! sessions are run.
//!
//! This is a separate crate so the code above it (session runtime, lifecycle
//! operations, the daemon) can be split into crates that each depend on the
//! store without depending on each other.

// The moved modules name these by their old paths.
pub(crate) use mj_targets::targets;
#[cfg(test)]
pub(crate) use mj_targets::{test_isolation, test_log};

pub mod database;
pub mod mailbox_outbox;
pub mod managed_archive;
pub mod recovery_gate;
pub mod upgrade;
pub mod web_push;
pub mod worker_lifecycle;
