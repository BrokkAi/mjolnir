//! Putting what a session needs onto its target before its worker starts and
//! keeping it current afterwards: selecting, verifying and installing the
//! worker binary, installing the harness, transferring the profile, and the
//! commands that probe, stop and restart a staged worker.
//!
//! It fills in the recovery plans of `mj-session-types` and builds on the
//! targets, store and accounts crates. The session runtime and
//! the controller's lifecycle operations use it; it uses neither.

// `test-support` compiles the test-only items for the tests of the crates
// above, which use only some of them. The dead-code lints still run on normal
// and test builds.
#![cfg_attr(
    all(feature = "test-support", not(test)),
    allow(dead_code, unused_imports)
)]

// The moved modules name these by their old paths.
pub(crate) use mj_accounts::{harness_auth, opencode_logins};
pub(crate) use mj_store::{database, worker_lifecycle};
pub(crate) use mj_targets::{aws_credentials, target_storage, targets};
#[cfg(any(test, feature = "test-support"))]
pub(crate) use mj_targets::{test_isolation, test_log};

pub mod worker_binary;
