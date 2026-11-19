//! What a session's worker is launched with, planned from the configuration
//! and the session records: its launch configuration, its files on the
//! target, its recovery plan, and the command that reconnects to it.
//!
//! These are functions over `Config` and `State`, so the session runtime can
//! plan a recovery and the controller a launch from the same code. The crate
//! builds on worker staging and the build caches.

// `test-support` compiles the test-only items for the tests of the crates
// above, which use only some of them. The dead-code lints still run on normal
// and test builds.
#![cfg_attr(
    all(feature = "test-support", not(test)),
    allow(dead_code, unused_imports)
)]

// The moved modules name these by their old paths.
pub(crate) use mj_store::{database, worker_lifecycle};
pub(crate) use mj_targets::{aws_assume_role, aws_credentials, targets};

pub mod launch_timing;
pub mod local_profile_homes;
pub mod worker_launch;
