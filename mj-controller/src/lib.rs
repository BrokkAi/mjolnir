//! Mjolnir's controller: provisioning, session management, storage, and the
//! host-side services that the daemon in `mj-daemon` drives.

// `test-support` compiles the test-only items for mj-daemon's tests, which use
// only some of them. The dead-code lints still run on normal and test builds.
#![cfg_attr(
    all(feature = "test-support", not(test)),
    allow(dead_code, unused_imports)
)]

mod antigravity_usage;
mod aws_assume_role;
mod aws_credentials;
pub mod backoff;
mod claude_login;
pub mod claude_usage;
mod codex_usage;
mod grok_usage;
mod kimi_auth;
pub mod launch_timing;
pub mod login_terminal;
mod muse_usage;
pub mod quota_runtime;
mod zai_usage;

pub mod compaction;
pub mod controller;
pub mod doctor;
pub mod handoff;
pub mod harness_login;
pub mod image;
pub mod import;
pub mod opencode_logins;
pub mod profile_catalog;
pub mod profile_health;
pub mod profile_login;
pub mod profile_scheduler;
pub mod project_catalog;
pub mod project_picker;
pub mod quota;
pub mod readline;
pub mod review_selection;
pub mod session_manager;
pub mod sessionwiki;
pub mod setup;
pub mod signing_key;
pub mod streamed_child;
pub mod tailscale;
pub mod utility_llm;
pub mod worker_client;
pub mod worker_lifecycle;

pub mod database;

pub mod target_storage;
pub mod targets;

pub mod termination;

pub(crate) mod project_memory_merge;

pub mod checkpoint_transfer;

pub mod image_pull_gate;
pub mod recovery_gate;

pub mod github_item_verdict;
pub mod github_rate_limit;
pub mod github_relevance;
mod jev_transport;
pub mod mailbox_outbox;
#[cfg(any(test, feature = "test-support"))]
pub mod test_isolation;
#[cfg(any(test, feature = "test-support"))]
pub mod test_log;
pub mod upgrade;
pub mod web_push;

pub mod wait_policy;

pub mod github_student;

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
