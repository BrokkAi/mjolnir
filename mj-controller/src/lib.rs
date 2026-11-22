//! Mjolnir's controller: provisioning, session management, storage, and the
//! host-side services that the daemon in `mj-daemon` drives.

pub use mj_accounts::claude_usage;
pub use mj_accounts::login_terminal;
pub use mj_core::backoff;
pub use mj_worker_launch::launch_timing;
pub use mj_worker_launch::local_profile_homes;

pub use mj_import::compaction;
pub mod controller;
pub use mj_accounts::harness_login;
pub use mj_import::handoff;
pub mod image;
pub use mj_accounts::opencode_logins;
pub use mj_accounts::power_ranking;
pub use mj_accounts::profile_health;
pub use mj_accounts::profile_login;
pub use mj_accounts::profile_scheduler;
pub use mj_accounts::quota;
pub use mj_accounts::quota_telemetry;
pub use mj_import::import;
pub use mj_import::project_catalog;
pub use mj_import::project_picker;
pub use mj_profile_catalog::profile_catalog;
pub use mj_profile_catalog::profile_config;
pub mod readline;
pub use mj_accounts::review_selection;
pub use mj_import::utility_llm;
pub use mj_session_runtime::session_manager;
pub use mj_session_runtime::worker_client;
pub use mj_sessionwiki::sessionwiki;
pub use mj_store::worker_lifecycle;
pub use mj_targets::signing_key;
pub use mj_targets::streamed_child;
pub use mj_targets::tailscale;

pub use mj_store::database;

pub use mj_targets::target_storage;
pub use mj_targets::targets;

pub use mj_targets::termination;

pub use mj_session_runtime::checkpoint_transfer;

pub use mj_store::recovery_gate;
pub use mj_targets::image_pull_gate;

pub use mj_session_runtime::github_app;
pub use mj_session_runtime::github_credential;
pub mod github_rate_limit;
pub use mj_store::mailbox_outbox;
pub use mj_store::upgrade;
pub use mj_store::web_push;
#[doc(hidden)]
pub use mj_targets::test_isolation;
#[doc(hidden)]
pub use mj_targets::test_log;

pub use mj_session_runtime::wait_policy;
pub use mj_worker_launch::worker_launch;

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
