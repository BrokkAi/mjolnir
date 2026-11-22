//! Which GitHub events matter to a session: the small classifier model that
//! runs on the daemon's machine, the download and verification of its weights
//! and native runtime, and the notices built from its verdicts.
//!
//! The Guardian also uses the crate's shared model cache and ONNX Runtime.

// `test-support` compiles the test-only items for the tests of the crates
// above, which use only some of them. The dead-code lints still run on normal
// and test builds.
#![cfg_attr(
    all(feature = "test-support", not(test)),
    allow(dead_code, unused_imports)
)]

// The moved tests name these by their old paths.
#[cfg(test)]
pub(crate) use mj_targets::{signing_key, test_isolation};

pub mod github_item_verdict;
pub mod github_relevance;
pub mod github_student;
pub mod guardian_classifier;
pub mod jev_transport;
