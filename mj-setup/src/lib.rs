//! What Mjolnir finds on a machine and what it can say is wrong with it:
//! first-run discovery of harnesses, logins, container engines and the
//! current project (`setup`), and the doctor's checks over a configured
//! installation (`doctor`).
//!
//! It sits above `mj-controller`, which it inspects and which does not use
//! it, so an edit here rebuilds neither the controller nor its tests.

// The moved modules name these by their old paths.
pub(crate) use mj_accounts::{harness_auth, opencode_logins, quota, zai_usage};
pub(crate) use mj_controller::controller;
pub(crate) use mj_import::import;
pub(crate) use mj_store::database;
pub(crate) use mj_targets::{aws_assume_role, aws_credentials, targets};

pub mod doctor;
pub mod setup;
