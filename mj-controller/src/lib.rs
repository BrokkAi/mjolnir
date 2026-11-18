//! Mjolnir's controller: provisioning, session management, storage, and the
//! host-side services that the daemon in `mj-daemon` drives.

// `test-support` compiles the test-only items for mj-daemon's tests, which use
// only some of them. The dead-code lints still run on normal and test builds.
#![cfg_attr(
    all(feature = "test-support", not(test)),
    allow(dead_code, unused_imports)
)]

pub(crate) use mj_targets::aws_assume_role;
pub(crate) use mj_targets::aws_credentials;
pub mod backoff;
pub(crate) use mj_accounts::claude_login;
pub use mj_accounts::claude_usage;
pub mod launch_timing;
pub use mj_accounts::login_terminal;
pub use mj_accounts::quota_runtime;

pub use mj_import::compaction;
pub mod controller;
pub(crate) use mj_accounts::harness_auth;
pub use mj_accounts::harness_login;
pub use mj_import::handoff;
pub mod image;
pub use mj_accounts::opencode_logins;
pub use mj_import::import;
pub mod profile_catalog;
pub use mj_accounts::profile_health;
pub use mj_accounts::profile_login;
pub use mj_accounts::profile_scheduler;
pub use mj_accounts::quota;
pub use mj_import::project_catalog;
pub use mj_import::project_picker;
pub mod readline;
pub use mj_accounts::review_selection;
pub mod session_manager;
pub use mj_import::utility_llm;
pub use mj_sessionwiki::sessionwiki;
pub use mj_targets::signing_key;
pub use mj_targets::streamed_child;
pub use mj_targets::tailscale;
pub mod worker_client;
pub use mj_store::worker_lifecycle;

pub use mj_store::database;

pub use mj_targets::target_storage;
pub use mj_targets::targets;

pub use mj_targets::termination;

pub(crate) mod project_memory_merge;

pub mod checkpoint_transfer;

pub use mj_store::recovery_gate;
pub use mj_targets::image_pull_gate;

pub mod github_rate_limit;
pub use mj_store::mailbox_outbox;
pub use mj_store::upgrade;
pub use mj_store::web_push;
#[cfg(any(test, feature = "test-support"))]
pub use mj_targets::test_isolation;
#[cfg(any(test, feature = "test-support"))]
pub use mj_targets::test_log;

pub mod wait_policy;

pub mod session_images;

// Stand-in relays that tests start by running this test binary again; see
// `mj_worker::stand_in_relay`. mj-controller's and mj-daemon's test binaries
// both define these, under the names in `mj_core::test_hooks`.
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
