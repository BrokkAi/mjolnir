//! The caches Mjolnir keeps on the host a session runs on, so that a new
//! session does not pay for work an earlier one already did: the mbx compiler
//! cache and its installation (`mbx`), the shared clone cache (`git_cache`),
//! the native build-tool caches (`tool_cache`), and the host they live on
//! (`cache_host`). It builds on the targets and store crates and knows nothing
//! of how a session is launched.

// `test-support` compiles the test-only items for the tests of the crates
// above, which use only some of them. The dead-code lints still run on normal
// and test builds.
#![cfg_attr(
    all(feature = "test-support", not(test)),
    allow(dead_code, unused_imports)
)]

// The moved modules name these by their old paths.
pub(crate) use mj_import::import;
pub(crate) use mj_store::database;
#[cfg(test)]
pub(crate) use mj_store::upgrade;
pub(crate) use mj_targets::targets;
#[cfg(any(test, feature = "test-support"))]
pub(crate) use mj_targets::test_isolation;

pub mod cache_host;
pub mod git_cache;
pub mod mbx;
pub mod tool_cache;
