//! The caches Mjolnir keeps on the host a session runs on, so that a new
//! session does not pay for work an earlier one already did: the mbx compiler
//! cache and its installation (`mbx`), the shared clone cache (`git_cache`),
//! the native build-tool caches (`tool_cache`), and the host they live on
//! (`cache_host`). It builds on the targets and store crates and knows nothing
//! of how a session is launched.

#[cfg(test)]
mj_core::enable_test_fixtures_on_startup!();

// The moved modules name these by their old paths.
pub(crate) use mj_store::database;
pub(crate) use mj_targets::targets;
#[cfg(test)]
pub(crate) use mj_targets::test_isolation;

pub mod cache_host;
pub mod git_cache;
pub mod mbx;
pub mod tool_cache;
