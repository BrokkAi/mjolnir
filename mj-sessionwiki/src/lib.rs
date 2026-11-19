//! The index of session history that searches, the resume picker and
//! workers' history queries read: keeping it in sync with Mjolnir's own
//! sessions and with other tools' stores, and answering queries over it. It
//! builds on the targets, store and import crates and reads session records
//! straight from the store, so the session runtime above can call it.

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
pub(crate) use mj_targets::targets;

pub mod sessionwiki;
