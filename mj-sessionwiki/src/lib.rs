//! The index of session history that searches, the resume picker and
//! workers' history queries read: keeping it in sync with Mjolnir's own
//! sessions and with other tools' stores, and answering queries over it. It
//! builds on the targets, store and import crates and reads session records
//! straight from the store, so the session runtime above can call it.

#[cfg(test)]
mj_core::enable_test_fixtures_on_startup!();

// The moved modules name these by their old paths.
pub(crate) use mj_import::import;
pub(crate) use mj_store::database;
pub(crate) use mj_targets::targets;

pub mod sessionwiki;
