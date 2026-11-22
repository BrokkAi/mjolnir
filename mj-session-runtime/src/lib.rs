//! The daemon's side of a running session: the actors that hold each
//! session's relay connection and project its events (`session_manager`), the
//! relay client and credential sync (`worker_client`), worker recovery, and
//! the checkpoint transfers and GitHub credentials those need.
//!
//! It builds on worker staging, worker launch and the store, and executes the
//! recovery plans of `mj-session-types`. The controller's lifecycle operations
//! use it; it does not know them.

// The moved modules name these by their old paths.
pub(crate) use mj_accounts::{claude_login, harness_auth, opencode_logins, profile_health};
pub(crate) use mj_import::utility_llm;
pub(crate) use mj_sessionwiki::sessionwiki;
pub(crate) use mj_store::{database, mailbox_outbox, recovery_gate, upgrade, worker_lifecycle};
pub(crate) use mj_targets::{
    aws_assume_role, aws_credentials, streamed_child, target_storage, targets,
};
// Only tests that need Unix use these.
#[cfg(test)]
#[cfg_attr(not(unix), allow(unused_imports))]
pub(crate) use mj_targets::{test_isolation, test_log};

pub mod checkpoint_transfer;
pub mod github_app;
pub mod github_credential;
pub mod project_memory_merge;
pub mod readiness;
pub mod session_manager;
pub mod wait_policy;
pub mod worker_client;

// Stand-in relays that tests start by running this test binary again; see
// `mj_worker::stand_in_relay`. The test binaries of the crates whose tests
// start one each define these, under the names in `mj_core::test_hooks`.
#[cfg(test)]
mod stand_in_relay {
    #[test]
    fn latch_relay_child_serves_stdio() {
        mj_worker::stand_in_relay::serve_latch_relay();
    }

    #[test]
    fn leased_relay_child_serves_stdio() {
        mj_worker::stand_in_relay::serve_leased_relay();
    }
}
