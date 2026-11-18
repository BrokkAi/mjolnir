//! Mjolnir's daemon: the control-plane process (`mj daemon-run`), its HTTP API
//! and web viewer, and the daemon-owned services built on `mj-controller`.

pub mod daemon;
mod github_watch;
pub mod observe;
pub mod pollers;
pub mod recovery;
pub mod review_host;
pub use mj_server::server;
pub mod server_runtime;
pub mod web_viewer;
pub mod worker_upgrade;

#[cfg(test)]
mod controller_tests;

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
