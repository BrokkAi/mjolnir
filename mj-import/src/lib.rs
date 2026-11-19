//! Bringing work into Mjolnir and carrying it between sessions: importing
//! another tool's sessions, the catalog of projects and their repositories,
//! compacting a transcript with the utility model, and the handoff built from
//! it. It builds on the targets, store and accounts crates and knows nothing
//! of a running session.
//!
//! This is a separate crate so the session runtime, the lifecycle operations
//! and the daemon can each use it without rebuilding one another.

// `test-support` compiles the test-only items for the tests of the crates
// above, which use only some of them. The dead-code lints still run on normal
// and test builds.
#![cfg_attr(
    all(feature = "test-support", not(test)),
    allow(dead_code, unused_imports)
)]

// The moved modules name these by their old paths.
pub(crate) use mj_accounts::{kimi_auth, quota};
pub(crate) use mj_store::{database, upgrade};
pub(crate) use mj_targets::targets;
#[cfg(any(test, feature = "test-support"))]
pub(crate) use mj_targets::test_isolation;

pub mod compaction;
pub mod handoff;
pub mod import;
pub mod project_catalog;
pub mod project_picker;
pub mod utility_llm;
