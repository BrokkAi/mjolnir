//! What each profile's harness offers. `profile_config` asks a harness for its
//! models and options by starting it from a staged copy of the profile, and
//! caches the answer; `profile_catalog` keeps those answers current for every
//! profile and serves them to the daemon's surfaces.
//!
//! It builds on worker staging, the store and the accounts crate, and is a
//! sibling of the session runtime: neither uses the other.

// `test-support` compiles the test-only items for the tests of the crates
// above, which use only some of them. The dead-code lints still run on normal
// and test builds.
#![cfg_attr(
    all(feature = "test-support", not(test)),
    allow(dead_code, unused_imports)
)]

// The moved modules name these by their old paths.
pub(crate) use mj_accounts::{quota, review_selection};
#[cfg(test)]
pub(crate) use mj_store::database;
pub(crate) use mj_targets::targets;
#[cfg(test)]
pub(crate) use mj_targets::test_isolation;

pub mod profile_catalog;
pub mod profile_config;
